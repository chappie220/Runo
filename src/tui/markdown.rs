//! Markdown replies as terminal lines: headings, emphasis, lists, quotes,
//! links, tables and highlighted code blocks, wrapped to the window by
//! display width so Chinese text and code line up.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use super::highlight;

const HEADING: Color = Color::Cyan;
const LINK: Color = Color::LightBlue;
const QUOTE: Color = Color::Green;
const RULE: Color = Color::DarkGray;
const INLINE_CODE: Style = Style::new().fg(Color::LightYellow).bg(Color::Indexed(236));
const GUTTER: Style = Style::new().fg(Color::DarkGray);
const BULLETS: &[&str] = &["•", "◦", "▪"];

/// `text` as lines `width` columns wide, every line starting with `base`'s look.
pub fn render(text: &str, width: usize, base: Style) -> Vec<Line<'static>> {
    let mut r = Renderer {
        width: width.max(8),
        base,
        ..Renderer::default()
    };
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(text, options) {
        r.event(event);
    }
    r.flush();
    if let Some(code) = r.code.take() {
        // A fence still open while the reply streams.
        r.code_block(code);
    }
    while r.out.last().is_some_and(|l| l.width() == 0) {
        r.out.pop();
    }
    r.out
}

struct ListLevel {
    /// Next number of an ordered list.
    next: Option<u64>,
    /// Columns of this level's marker, which its text lines up after.
    indent: usize,
}

struct CodeBlock {
    lang: String,
    text: String,
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<Vec<Span<'static>>>>,
    row: Vec<Vec<Span<'static>>>,
    cell: Vec<Span<'static>>,
}

#[derive(Default)]
struct Renderer {
    width: usize,
    base: Style,
    out: Vec<Line<'static>>,
    /// The paragraph being collected.
    spans: Vec<Span<'static>>,
    styles: Vec<Style>,
    quote: usize,
    lists: Vec<ListLevel>,
    /// The marker of a list item whose first line is not out yet.
    marker: Option<String>,
    code: Option<CodeBlock>,
    /// Destination of each open link, and where its text starts in `spans`.
    links: Vec<(String, usize)>,
    table: Option<Table>,
    /// A blank line is due before the next block.
    gap: bool,
}

impl Renderer {
    fn style(&self) -> Style {
        self.styles
            .iter()
            .fold(self.base, |style, patch| style.patch(*patch))
    }

    fn push(&mut self, span: Span<'static>) {
        match &mut self.table {
            Some(table) => table.cell.push(span),
            None => self.spans.push(span),
        }
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => match &mut self.code {
                Some(code) => code.text.push_str(&text),
                None => {
                    let style = self.style();
                    self.push(Span::styled(text.into_string(), style));
                }
            },
            Event::Code(text) => self.push(Span::styled(text.into_string(), INLINE_CODE)),
            Event::SoftBreak | Event::HardBreak => {
                // Chat replies mean a newline when they write one.
                if self.table.is_some() {
                    self.push(Span::raw(" "));
                } else {
                    self.flush();
                }
            }
            Event::Rule => {
                self.block();
                let width = self.width.saturating_sub(self.prefix_width());
                let line = Span::styled("─".repeat(width), Style::new().fg(RULE));
                let mut spans = self.prefix(false);
                spans.push(line);
                self.out.push(Line::from(spans));
                self.gap = true;
            }
            Event::TaskListMarker(done) => {
                let (mark, color) = if done {
                    ("☑ ", Color::Green)
                } else {
                    ("☐ ", Color::DarkGray)
                };
                self.push(Span::styled(mark, Style::new().fg(color)));
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                self.push(Span::styled(
                    html.into_string(),
                    self.base.fg(Color::DarkGray),
                ));
            }
            Event::FootnoteReference(name) => self.push(Span::raw(format!("[^{name}]"))),
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.push(Span::styled(text.into_string(), INLINE_CODE));
            }
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => self.block(),
            Tag::Heading { level, .. } => {
                self.block();
                let style = match level {
                    HeadingLevel::H1 => Style::new()
                        .fg(HEADING)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                    HeadingLevel::H2 => Style::new().fg(HEADING).add_modifier(Modifier::BOLD),
                    HeadingLevel::H3 => Style::new().add_modifier(Modifier::BOLD),
                    _ => Style::new().add_modifier(Modifier::BOLD | Modifier::ITALIC),
                };
                self.styles.push(style);
            }
            Tag::BlockQuote(_) => {
                self.block();
                self.quote += 1;
                self.styles
                    .push(Style::new().fg(Color::Gray).add_modifier(Modifier::ITALIC));
            }
            Tag::CodeBlock(kind) => {
                self.block();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_owned()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some(CodeBlock {
                    lang,
                    text: String::new(),
                });
            }
            Tag::List(first) => {
                self.flush();
                if self.lists.is_empty() {
                    self.block();
                }
                self.lists.push(ListLevel {
                    next: first,
                    indent: 2,
                });
            }
            Tag::Item => {
                self.flush();
                let depth = self.lists.len().saturating_sub(1);
                let Some(level) = self.lists.last_mut() else {
                    return;
                };
                let marker = match &mut level.next {
                    Some(n) => {
                        *n += 1;
                        format!("{}. ", *n - 1)
                    }
                    None => format!("{} ", BULLETS[depth % BULLETS.len()]),
                };
                level.indent = marker.chars().count();
                self.marker = Some(marker);
            }
            Tag::Emphasis => self
                .styles
                .push(Style::new().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.styles.push(Style::new().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .styles
                .push(Style::new().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                let from = self.spans.len();
                self.links.push((dest_url.into_string(), from));
                self.styles
                    .push(Style::new().fg(LINK).add_modifier(Modifier::UNDERLINED));
            }
            Tag::Image { .. } => {
                self.push(Span::styled("[🖼 ", Style::new().fg(Color::DarkGray)));
                self.styles
                    .push(Style::new().add_modifier(Modifier::ITALIC));
            }
            Tag::Table(_) => {
                self.block();
                self.table = Some(Table::default());
            }
            Tag::TableHead | Tag::TableRow | Tag::TableCell => {}
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush();
                self.gap = true;
            }
            TagEnd::Heading(_) => {
                self.flush();
                self.styles.pop();
                self.gap = true;
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
                self.styles.pop();
                self.gap = true;
            }
            TagEnd::CodeBlock => {
                if let Some(code) = self.code.take() {
                    self.code_block(code);
                }
                self.gap = true;
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                self.gap = self.lists.is_empty();
            }
            TagEnd::Item => {
                self.flush();
                self.marker = None;
                self.gap = false;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some((dest, from)) = self.links.pop() {
                    let text: String = self
                        .spans
                        .get(from..)
                        .unwrap_or_default()
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect();
                    if !dest.is_empty() && text.trim() != dest && !dest.starts_with('#') {
                        self.push(Span::styled(
                            format!(" ({dest})"),
                            Style::new().fg(Color::DarkGray),
                        ));
                    }
                }
            }
            TagEnd::Image => {
                self.styles.pop();
                self.push(Span::styled("]", Style::new().fg(Color::DarkGray)));
            }
            TagEnd::TableCell => {
                if let Some(t) = &mut self.table {
                    let cell = std::mem::take(&mut t.cell);
                    t.row.push(cell);
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.row);
                    t.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.render_table(table);
                }
                self.gap = true;
            }
            _ => {}
        }
    }

    /// A new block starts: the blank line before it, when one is due.
    fn block(&mut self) {
        self.flush();
        if self.gap && !self.out.is_empty() {
            let prefix = self.prefix(false);
            self.out.push(Line::from(prefix));
        }
        self.gap = false;
    }

    /// Quote bars and list indentation; `first` puts the item marker on it.
    fn prefix(&mut self, first: bool) -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        for _ in 0..self.quote {
            spans.push(Span::styled("│ ", Style::new().fg(QUOTE)));
        }
        let levels = self.lists.len();
        let outer: usize = self
            .lists
            .iter()
            .take(levels.saturating_sub(1))
            .map(|l| l.indent)
            .sum();
        if outer > 0 {
            spans.push(Span::raw(" ".repeat(outer)));
        }
        if let Some(level) = self.lists.last() {
            match self.marker.take().filter(|_| first) {
                Some(marker) => spans.push(Span::styled(marker, Style::new().fg(HEADING))),
                None => spans.push(Span::raw(" ".repeat(level.indent))),
            }
        }
        spans
    }

    fn prefix_width(&self) -> usize {
        self.quote * 2 + self.lists.iter().map(|l| l.indent).sum::<usize>()
    }

    /// Writes out the collected paragraph, wrapped.
    fn flush(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.spans);
        let first = self.prefix(true);
        let rest = self.prefix(false);
        self.out
            .extend(wrap_spans(&spans, self.width, first, rest, true));
    }

    fn code_block(&mut self, code: CodeBlock) {
        let mut head = self.prefix(true);
        let rest = self.prefix(false);
        let label = if code.lang.is_empty() {
            "code".to_owned()
        } else {
            code.lang.clone()
        };
        head.push(Span::styled(
            format!("╭─ {label}"),
            GUTTER.add_modifier(Modifier::ITALIC),
        ));
        self.out.push(Line::from(head));
        let mut state = highlight::State::default();
        let text = code.text.strip_suffix('\n').unwrap_or(&code.text);
        for row in text.split('\n') {
            let row = row.replace('\t', "    ");
            let spans = highlight::line(&row, &code.lang, &mut state);
            let mut gutter = rest.clone();
            gutter.push(Span::styled("│ ", GUTTER));
            self.out.extend(wrap_spans(
                &spans,
                self.width,
                gutter.clone(),
                gutter,
                false,
            ));
        }
        let mut foot = rest;
        foot.push(Span::styled("╰─", GUTTER));
        self.out.push(Line::from(foot));
    }

    fn render_table(&mut self, table: Table) {
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let cell_width = |cell: &[Span]| cell.iter().map(|s| s.width()).sum::<usize>();
        let mut widths = vec![1usize; columns];
        for row in &table.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell_width(cell));
            }
        }
        // Borders take 3 columns per cell and 1 more; narrow the widest
        // columns until the table fits.
        let available = self
            .width
            .saturating_sub(self.prefix_width() + 3 * columns + 1)
            .max(columns * 3);
        while widths.iter().sum::<usize>() > available {
            let widest = (0..columns).max_by_key(|&i| widths[i]).unwrap_or(0);
            if widths[widest] <= 3 {
                break;
            }
            widths[widest] -= 1;
        }
        let border = Style::new().fg(Color::DarkGray);
        let rule = |left: &str, mid: &str, right: &str| {
            let parts: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
            Span::styled(format!("{left}{}{right}", parts.join(mid)), border)
        };
        let mut lines = vec![vec![rule("┌", "┬", "┐")]];
        for (r, row) in table.rows.iter().enumerate() {
            let mut spans = vec![Span::styled("│", border)];
            for (i, width) in widths.iter().enumerate() {
                let cell = row.get(i).map(Vec::as_slice).unwrap_or_default();
                let mut cut = truncate_spans(cell, *width);
                if r == 0 {
                    for span in &mut cut {
                        span.style = span.style.add_modifier(Modifier::BOLD);
                    }
                }
                let used: usize = cut.iter().map(|s| s.width()).sum();
                spans.push(Span::raw(" "));
                spans.extend(cut);
                spans.push(Span::raw(" ".repeat(width - used.min(*width) + 1)));
                spans.push(Span::styled("│", border));
            }
            lines.push(spans);
            if r == 0 && table.rows.len() > 1 {
                lines.push(vec![rule("├", "┼", "┤")]);
            }
        }
        lines.push(vec![rule("└", "┴", "┘")]);
        for spans in lines {
            let mut line = self.prefix(false);
            line.extend(spans);
            self.out.push(Line::from(line));
        }
    }
}

fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

/// Styled text cut to `width` columns, with … when it was longer.
fn truncate_spans(spans: &[Span<'static>], width: usize) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(|s| s.width()).sum();
    if total <= width {
        return spans.to_vec();
    }
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let mut text = String::new();
        for c in span.content.chars() {
            let w = char_width(c);
            if used + w + 1 > width {
                if !text.is_empty() {
                    out.push(Span::styled(text, span.style));
                }
                out.push(Span::styled("…", span.style));
                return out;
            }
            text.push(c);
            used += w;
        }
        out.push(Span::styled(text, span.style));
    }
    out
}

/// Breaks styled text into lines of at most `width` columns, the first
/// starting with `first` and the rest with `rest`. With `words`, lines break
/// at spaces or between wide (CJK) characters when they can; otherwise
/// anywhere, as code needs.
pub fn wrap_spans(
    spans: &[Span<'static>],
    width: usize,
    first: Vec<Span<'static>>,
    rest: Vec<Span<'static>>,
    words: bool,
) -> Vec<Line<'static>> {
    let prefix_width = |p: &[Span]| p.iter().map(|s| s.width()).sum::<usize>();
    let chars: Vec<(char, Style)> = spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .filter(|(c, _)| *c != '\n' && *c != '\r')
        .collect();
    let mut lines = Vec::new();
    let mut row: Vec<(char, Style)> = Vec::new();
    let mut used = 0;
    let mut prefix = first;
    let room = |prefix: &[Span]| width.saturating_sub(prefix_width(prefix)).max(4);
    let emit = |prefix: &[Span<'static>], row: &[(char, Style)], lines: &mut Vec<Line<'static>>| {
        let mut out: Vec<Span<'static>> = prefix.to_vec();
        let mut text = String::new();
        let mut style = None;
        for &(c, s) in row {
            if style.is_some_and(|st| st != s) {
                out.push(Span::styled(
                    std::mem::take(&mut text),
                    style.unwrap_or_default(),
                ));
            }
            style = Some(s);
            text.push(c);
        }
        if !text.is_empty() {
            out.push(Span::styled(text, style.unwrap_or_default()));
        }
        lines.push(Line::from(out));
    };
    for (c, style) in chars {
        let w = char_width(c);
        if used + w > room(&prefix) && !row.is_empty() {
            // The last place this row may break: after a space, or between wide characters.
            // Before position k (`row.len()` is before `c`) when a space is
            // there or a wide character is on either side.
            let breaks = |k: usize| {
                let next = row.get(k).map_or(c, |&(c, _)| c);
                next == ' ' || char_width(row[k - 1].0) > 1 || char_width(next) > 1
            };
            let at = words
                .then(|| (1..=row.len()).rev().find(|&k| breaks(k)))
                .flatten();
            let carry = match at {
                Some(k) => row.split_off(k),
                None => Vec::new(),
            };
            while words && row.last().is_some_and(|(c, _)| *c == ' ') {
                row.pop();
            }
            emit(&prefix, &row, &mut lines);
            row = carry;
            while words && row.first().is_some_and(|(c, _)| *c == ' ') {
                row.remove(0);
            }
            prefix = rest.clone();
            used = row.iter().map(|(c, _)| char_width(*c)).sum();
        }
        if words && row.is_empty() && c == ' ' && !lines.is_empty() {
            continue;
        }
        row.push((c, style));
        used += w;
    }
    if !row.is_empty() || lines.is_empty() {
        emit(&prefix, &row, &mut lines);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn plain(md: &str, width: usize) -> Vec<String> {
        text(&render(md, width, Style::new()))
    }

    #[test]
    fn renders_blocks_without_markup() {
        let out = plain(
            "# Title\n\nSome **bold** and `code`.\n\n- one\n- two\n  1. nested\n\n> quoted\n\n---",
            30,
        );
        assert_eq!(
            out,
            [
                "Title",
                "",
                "Some bold and code.",
                "",
                "• one",
                "• two",
                "  1. nested",
                "",
                "│ quoted",
                "",
                "──────────────────────────────",
            ]
        );
        let styled = render("**b** _i_", 20, Style::new());
        assert!(
            styled[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            styled[0].spans[2]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
    }

    #[test]
    fn wraps_words_and_chinese_under_list_markers() {
        assert_eq!(
            plain("- alpha beta gamma delta", 14),
            ["• alpha beta", "  gamma delta"]
        );
        assert_eq!(plain("中文中文中文中文", 8), ["中文中文", "中文中文"]);
        // A newline in a reply stays a newline.
        assert_eq!(plain("line one\nline two", 40), ["line one", "line two"]);
    }

    #[test]
    fn code_blocks_keep_lines_and_streaming_fences_still_show() {
        assert_eq!(
            plain("```rust\nfn main() {}\n    x\n```", 40),
            ["╭─ rust", "│ fn main() {}", "│     x", "╰─"]
        );
        // The closing fence has not arrived yet.
        assert_eq!(plain("```\nabc", 40), ["╭─ code", "│ abc", "╰─"]);
        let long = plain("```\nabcdefghij\n```", 8);
        assert_eq!(long[1..3], ["│ abcdef", "│ ghij"]);
    }

    #[test]
    fn tables_fit_the_width_and_links_show_where_they_go() {
        let out = plain("| a | bb |\n|---|---|\n| 1 | 22 |", 40);
        assert_eq!(
            out,
            [
                "┌───┬────┐",
                "│ a │ bb │",
                "├───┼────┤",
                "│ 1 │ 22 │",
                "└───┴────┘"
            ]
        );
        let narrow = plain("| long header | x |\n|---|---|\n| v | y |", 16);
        assert!(narrow.iter().all(|l| l.chars().count() <= 16), "{narrow:?}");
        assert_eq!(
            plain("[docs](https://x.org) and <https://y.org>", 60),
            ["docs (https://x.org) and https://y.org"]
        );
    }
}
