//! Drawing the window: the session list on the left, the conversation and
//! input in one panel on the right, a status line below, and popups for
//! completions, the command palette and approvals.

use std::hash::{Hash, Hasher};
use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::markdown::{self, wrap_spans};
use super::*;

const ACCENT: Color = Color::Cyan;
const USER: Color = Color::Green;
const MUTED: Color = Color::DarkGray;
const WARN: Color = Color::Yellow;
const ERROR: Color = Color::Red;
const BORDER: Color = Color::Indexed(240);
const USER_BG: Color = Color::Indexed(235);
const SELECTED_BG: Color = Color::Indexed(238);
/// Columns of the session list: more on wide terminals.
const SIDEBAR_WIDTH: u16 = 26;
const SIDEBAR_WIDE: u16 = 32;
/// Cells in the context gauge.
const GAUGE: usize = 8;
/// Rows of the Ctrl+K palette's list.
const PALETTE_ROWS: usize = 12;

fn muted() -> Style {
    Style::new().fg(MUTED)
}

/// "0.4s", "12s", "3m05s".
fn seconds(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 10.0 {
        format!("{s:.1}s")
    } else if s < 60.0 {
        format!("{}s", s as u64)
    } else {
        format!("{}m{:02}s", s as u64 / 60, s as u64 % 60)
    }
}

/// `text` cut to `max` columns with …, kept on one line.
fn fit(text: &str, max: usize) -> String {
    if text.width() <= max {
        text.to_owned()
    } else {
        one_line(text, max)
    }
}

/// `left` and `right` on one line `width` wide, `right` against the edge.
fn spread(mut left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let used: usize = left.iter().chain(&right).map(|s| s.width()).sum();
    left.push(Span::raw(" ".repeat(width.saturating_sub(used))));
    left.extend(right);
    Line::from(left)
}

impl App {
    pub(super) fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        let mut hits = Hits::default();
        let [top, footer] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(area);
        let main = if self.sidebar_shown(area.width) {
            let width = if area.width >= 120 {
                SIDEBAR_WIDE
            } else {
                SIDEBAR_WIDTH
            };
            let [side, main] =
                Layout::horizontal([Constraint::Length(width), Constraint::Min(20)]).areas(top);
            self.draw_sidebar(frame, side, &mut hits);
            main
        } else {
            top
        };

        let focused = self.focus == Focus::Input && self.palette.is_none();
        let border = if focused && !self.running {
            ACCENT
        } else {
            BORDER
        };
        let title = Line::from(vec![
            Span::styled(
                format!(" {} {} ", self.icon, self.name),
                Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("· {} ", self.session), Style::new().fg(Color::Gray)),
        ]);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(border))
            .title(title)
            .title(self.model_title().alignment(Alignment::Right));
        let inner = block.inner(main);
        frame.render_widget(block, main);
        // One column of air on each side of the text.
        let inner = Rect {
            x: inner.x + 1,
            width: inner.width.saturating_sub(2),
            ..inner
        };

        // The input: "› " then the text, wrapped.
        let input_width = usize::from(inner.width.saturating_sub(2)).max(4);
        let (rows, cursor) = self.input.layout(input_width);
        let shown = rows.len().clamp(1, MAX_INPUT_LINES);
        let [body, separator, input] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(shown as u16),
        ])
        .areas(inner);

        hits.body = body;
        if let Some(manage) = &self.manage {
            self.draw_manage(frame, body, manage);
        } else {
            let width = usize::from(body.width.max(10));
            let (lines, owners) = self.lines(width);
            let height = usize::from(body.height);
            let bottom = lines.len().saturating_sub(height);
            let top = bottom.saturating_sub(self.scroll.min(bottom));
            let visible: Vec<Line> = lines.into_iter().skip(top).take(height).collect();
            hits.body_rows = owners.into_iter().skip(top).take(height).collect();
            frame.render_widget(Paragraph::new(visible), body);
        }

        // The separator joins the panel's border: ├────┤.
        let rule = format!(
            "├{}┤",
            "─".repeat(usize::from(main.width.saturating_sub(2)))
        );
        frame.render_widget(
            Paragraph::new(Line::styled(rule, Style::new().fg(border))),
            Rect::new(main.x, separator.y, main.width, 1),
        );

        let first = (cursor.0 + 1).saturating_sub(shown);
        let prompt = Style::new().fg(if self.running { MUTED } else { ACCENT });
        let mut text: Vec<Line> = rows
            .iter()
            .enumerate()
            .skip(first)
            .take(shown)
            .map(|(i, r)| {
                let lead = if i == 0 { "› " } else { "  " };
                Line::from(vec![Span::styled(lead, prompt), Span::raw(r.clone())])
            })
            .collect();
        if self.input.is_empty() && focused && self.approval.is_none() {
            let hint = PLACEHOLDER.fill(self.lang, &[&self.name]);
            text = vec![Line::from(vec![
                Span::styled("› ", prompt),
                Span::styled(
                    fit(&hint, input_width),
                    muted().add_modifier(Modifier::ITALIC),
                ),
            ])];
        }
        frame.render_widget(Paragraph::new(text), input);
        if focused && self.approval.is_none() {
            frame.set_cursor_position(Position::new(
                input.x + 2 + cursor.1 as u16,
                input.y + (cursor.0 - first) as u16,
            ));
        }

        self.draw_footer(frame, footer);

        if let Some((tool, summary, _)) = &self.approval {
            self.draw_approval(frame, area, tool, summary);
        } else if let Some(palette) = &self.palette {
            self.draw_palette(frame, area, palette);
        } else if let Some(c) = &self.completion {
            Self::draw_completion(frame, body, input, c);
        }
        *self.hits.borrow_mut() = hits;
    }

    /// The model's name for the panel's title, marked when it is down or
    /// being checked.
    fn model_title(&self) -> Line<'static> {
        let (mark, style) = match &self.health {
            Health::Down(_) => ("✗ ", Style::new().fg(ERROR)),
            Health::Checking => ("… ", muted()),
            _ => ("", muted()),
        };
        Line::from(vec![
            Span::styled(format!(" {mark}"), style),
            Span::styled(format!("{} ", self.model), style),
        ])
    }

    /// The management view in the conversation's place: the model's state,
    /// the menu, and below it what the last item showed, its end in view.
    fn draw_manage(&self, frame: &mut Frame, area: Rect, manage: &Manage) {
        let width = usize::from(area.width.max(10));
        let focused = self.focus == Focus::Manage && self.palette.is_none();
        let mut head = vec![Line::styled(
            fit(
                &format!(
                    "{} · {}",
                    manage::TITLE.get(self.lang),
                    self.health.line(self.lang, &self.model)
                ),
                width,
            ),
            match self.health {
                Health::Down(_) => Style::new().fg(ERROR).add_modifier(Modifier::BOLD),
                _ => Style::new().add_modifier(Modifier::BOLD),
            },
        )];
        if self.health.is_down() {
            for row in wrap(Health::note(self.lang), width) {
                head.push(Line::styled(row, muted()));
            }
        }
        head.push(Line::raw(""));
        for (i, item) in ITEMS.iter().enumerate() {
            let selected = i == manage.selected;
            let mark = if selected { "▸ " } else { "  " };
            let style = if selected && focused {
                Style::new()
                    .fg(ACCENT)
                    .add_modifier(Modifier::BOLD)
                    .bg(SELECTED_BG)
            } else if selected {
                Style::new().fg(ACCENT)
            } else {
                Style::new()
            };
            head.push(Line::styled(
                fit(&format!("{mark}{} {}", i + 1, item.label(self.lang)), width),
                style,
            ));
        }
        let rows = (head.len() as u16).min(area.height);
        let [top, rest] =
            Layout::vertical([Constraint::Length(rows), Constraint::Min(0)]).areas(area);
        frame.render_widget(Paragraph::new(head), top);
        let Some((title, detail)) = &manage.detail else {
            return;
        };
        let mut lines = vec![
            Line::raw(""),
            Line::styled(fit(&format!("── {title} ──"), width), muted()),
        ];
        for row in detail {
            lines.extend(wrap(row, width).into_iter().map(Line::raw));
        }
        let height = usize::from(rest.height);
        let bottom = lines.len().saturating_sub(height);
        let first = bottom.saturating_sub(self.scroll.min(bottom));
        let visible: Vec<Line> = lines.into_iter().skip(first).take(height).collect();
        frame.render_widget(Paragraph::new(visible), rest);
    }

    fn draw_sidebar(&self, frame: &mut Frame, area: Rect, hits: &mut Hits) {
        let focused = self.focus == Focus::Sidebar && self.palette.is_none();
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(if focused { ACCENT } else { BORDER }))
            .title(Span::styled(
                format!(" {} ", SESSIONS.get(self.lang)),
                Style::new().add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height < 2 {
            return;
        }
        let [list, hint] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
        let rows = usize::from(list.height);
        let width = usize::from(list.width);
        let anchor = if focused {
            self.sidebar_selected
        } else {
            self.sessions
                .iter()
                .position(|s| s.name == self.session)
                .unwrap_or(0)
        };
        let offset = anchor.saturating_sub(rows.saturating_sub(1));
        let mut lines = Vec::new();
        for (i, s) in self.sessions.iter().enumerate().skip(offset).take(rows) {
            let current = s.name == self.session;
            let count = if s.messages > 0 {
                format!("{} ", s.messages)
            } else {
                String::new()
            };
            let name_width = width.saturating_sub(2 + count.width() + 1);
            let (mark, style) = if current {
                ("▸ ", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD))
            } else {
                ("  ", Style::new())
            };
            let mut line = spread(
                vec![
                    Span::styled(mark, style),
                    Span::styled(fit(&s.name, name_width), style),
                ],
                vec![Span::styled(count, muted())],
                width,
            );
            if focused && i == self.sidebar_selected {
                line = line.style(Style::new().bg(SELECTED_BG));
            }
            lines.push(line);
            hits.sidebar_rows.push(s.name.clone());
        }
        frame.render_widget(Paragraph::new(lines), list);
        hits.sidebar = list;
        hits.sidebar_first_row = list.y;
        frame.render_widget(
            Paragraph::new(Line::styled(fit(" ^N + · Tab ⇄", width), muted())),
            hint,
        );
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let mut left = Vec::new();
        if self.running {
            let spin = SPINNER[self.spinner % SPINNER.len()];
            let text = if self.stopping { STOPPING } else { WORKING };
            left.push(Span::styled(
                format!(" {spin} {}", text.get(self.lang)),
                Style::new().fg(WARN),
            ));
        }
        if !self.queue.is_empty() {
            left.push(Span::styled(
                format!(
                    "  {}",
                    QUEUED.fill(self.lang, &[&self.queue.len().to_string()])
                ),
                Style::new().fg(Color::Magenta),
            ));
        }
        if !self.files.is_empty() {
            let names: Vec<&str> = self.files.iter().map(|f| f.name.as_str()).collect();
            left.push(Span::styled(
                format!("  📎 {}", FILES.fill(self.lang, &[&names.join(", ")])),
                Style::new().fg(Color::Blue),
            ));
        }
        if self.scroll > 0 {
            left.push(Span::styled(format!("  ↑{}", self.scroll), muted()));
        }
        if left.is_empty() {
            let hints = match self.focus {
                Focus::Sidebar => SIDEBAR_HINTS,
                Focus::Manage => manage::HINTS,
                Focus::Input => HINTS,
            };
            left.push(Span::styled(format!(" {}", hints.get(self.lang)), muted()));
        }
        let mut right = Vec::new();
        if let Some((used, budget)) = self.context {
            let percent = used * 100 / budget.max(1);
            let color = match percent {
                0..60 => Color::Green,
                60..85 => WARN,
                _ => ERROR,
            };
            let filled = (percent * GAUGE).div_ceil(100).min(GAUGE);
            right.push(Span::styled(
                CONTEXT.fill(
                    self.lang,
                    &[
                        &crate::config::short_tokens(used),
                        &crate::config::short_tokens(budget),
                    ],
                ) + " ",
                muted(),
            ));
            right.push(Span::styled("▰".repeat(filled), Style::new().fg(color)));
            right.push(Span::styled("▱".repeat(GAUGE - filled), muted()));
            right.push(Span::styled(
                format!(" {percent}% "),
                Style::new().fg(color),
            ));
        }
        let width = usize::from(area.width);
        let right_width: usize = right.iter().map(|s| s.width()).sum();
        // The hints give way to the gauge on narrow terminals.
        let room = width.saturating_sub(right_width + 1);
        let text: String = left.iter().map(|s| s.content.as_ref()).collect();
        if text.width() > room {
            let style = left.first().map(|s| s.style).unwrap_or_default();
            let cut = fit(text.trim_start(), room.saturating_sub(1));
            left = vec![Span::styled(format!(" {cut}"), style)];
        }
        frame.render_widget(Paragraph::new(spread(left, right, width)), area);
    }

    /// The @ or / list, just above the input.
    fn draw_completion(frame: &mut Frame, body: Rect, input: Rect, c: &Completion) {
        let rows = c.items.len().min(MAX_SUGGESTIONS);
        let height = (rows as u16 + 2).min(body.height + 1);
        let width = body.width.min(76);
        let popup = Rect::new(
            input.x.saturating_sub(1),
            input.y.saturating_sub(height + 1),
            width,
            height,
        );
        let inner = usize::from(width.saturating_sub(4));
        let sigil = if c.kind == Kind::Command { "/" } else { "@" };
        let name_width = c
            .items
            .iter()
            .map(|(n, _)| n.chars().count() + 2)
            .max()
            .unwrap_or(0)
            .min(inner / 2);
        let offset = c.selected.saturating_sub(rows.saturating_sub(1));
        let lines: Vec<Line> = c
            .items
            .iter()
            .enumerate()
            .skip(offset)
            .take(rows)
            .map(|(i, (name, description))| {
                if name.is_empty() {
                    return Line::styled(fit(description, inner), muted());
                }
                let label = format!("{sigil}{name}");
                let pad = " ".repeat(name_width.saturating_sub(label.chars().count()) + 1);
                let line = Line::from(vec![
                    Span::styled(label, Style::new().fg(ACCENT)),
                    Span::raw(pad),
                    Span::styled(
                        fit(description, inner.saturating_sub(name_width + 1)),
                        muted(),
                    ),
                ]);
                if i == c.selected {
                    line.style(Style::new().bg(SELECTED_BG).add_modifier(Modifier::BOLD))
                } else {
                    line
                }
            })
            .collect();
        let title = if c.kind == Kind::Command {
            " commands · Tab "
        } else {
            " skills · Tab "
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(ACCENT))
            .title(Span::styled(title, muted()));
        frame.render_widget(Clear, popup);
        frame.render_widget(Paragraph::new(lines).block(block), popup);
    }

    fn draw_palette(&self, frame: &mut Frame, area: Rect, palette: &Palette) {
        let query = palette.query.string();
        let items = self.palette_items(&query);
        let rows = items.len().clamp(1, PALETTE_ROWS);
        let width = area.width.saturating_sub(4).min(72);
        let height = (rows as u16 + 4).min(area.height);
        let popup = Rect::new(
            area.x + (area.width.saturating_sub(width)) / 2,
            area.y + (area.height / 6).min(area.height.saturating_sub(height)),
            width,
            height,
        );
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(ACCENT))
            .title(Span::styled(
                format!(" {} ", PALETTE.get(self.lang)),
                Style::new().add_modifier(Modifier::BOLD),
            ))
            .title(Line::from(Span::styled(" Esc ", muted())).alignment(Alignment::Right));
        let inner = block.inner(popup);
        frame.render_widget(Clear, popup);
        frame.render_widget(block, popup);
        let [field, rule, list] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .areas(inner);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" › ", Style::new().fg(ACCENT)),
                Span::raw(query),
            ])),
            field,
        );
        frame.render_widget(
            Paragraph::new(Line::styled("─".repeat(usize::from(rule.width)), muted())),
            rule,
        );
        let width = usize::from(list.width);
        let selected = palette.selected.min(items.len().saturating_sub(1));
        let offset = selected.saturating_sub(rows - 1);
        let mut lines: Vec<Line> = items
            .iter()
            .enumerate()
            .skip(offset)
            .take(rows)
            .map(|(i, item)| {
                let label_style = if item.label.starts_with('/') {
                    Style::new().fg(ACCENT)
                } else {
                    Style::new()
                };
                let label = fit(&item.label, width / 2);
                let line = spread(
                    vec![Span::raw(" "), Span::styled(label.clone(), label_style)],
                    vec![Span::styled(
                        format!(
                            "{} ",
                            fit(&item.detail, width.saturating_sub(label.width() + 4))
                        ),
                        muted(),
                    )],
                    width,
                );
                if i == selected {
                    line.style(Style::new().bg(SELECTED_BG).add_modifier(Modifier::BOLD))
                } else {
                    line
                }
            })
            .collect();
        if lines.is_empty() {
            lines.push(Line::styled(
                format!(" {}", NO_MATCH.get(self.lang)),
                muted(),
            ));
        }
        frame.render_widget(Paragraph::new(lines), list);
        frame.set_cursor_position(Position::new(
            field.x
                + 3
                + palette.query.text[..palette.query.cursor]
                    .iter()
                    .map(|c| c.width().unwrap_or(0) as u16)
                    .sum::<u16>(),
            field.y,
        ));
    }

    fn draw_approval(&self, frame: &mut Frame, area: Rect, tool: &str, summary: &str) {
        let width = area.width.saturating_sub(4).clamp(20, 90);
        let inner = usize::from(width.saturating_sub(4));
        let mut lines: Vec<Line> = wrap(summary, inner)
            .into_iter()
            .take(usize::from(area.height.saturating_sub(6)).max(1))
            .map(Line::raw)
            .collect();
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            ALLOW_KEYS.get(self.lang),
            Style::new().fg(WARN).add_modifier(Modifier::BOLD),
        ));
        let height = (lines.len() as u16 + 2).min(area.height);
        let popup = Rect::new(
            area.x + (area.width.saturating_sub(width)) / 2,
            area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
        );
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(WARN))
            .title(Span::styled(
                format!(" ⚠ {} ", ALLOW.fill(self.lang, &[tool])),
                Style::new().fg(WARN).add_modifier(Modifier::BOLD),
            ));
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines).block(block.padding(ratatui::widgets::Padding::horizontal(1))),
            popup,
        );
    }

    /// An assistant message as Markdown lines, from the cache when unchanged.
    fn markdown(&self, index: usize, text: &str, width: usize) -> Vec<Line<'static>> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        let hash = hasher.finish();
        let mut cache = self.cache.borrow_mut();
        if let Some(c) = cache.get(&index)
            && c.width == width
            && c.hash == hash
        {
            return c.lines.clone();
        }
        let lines = markdown::render(text, width, Style::new());
        cache.insert(
            index,
            Cached {
                width,
                hash,
                lines: lines.clone(),
            },
        );
        lines
    }

    /// Every entry as screen lines `width` columns wide, and the entry each
    /// line belongs to when a click on it does something (tool lines).
    pub(super) fn lines(&self, width: usize) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
        let text_width = width.saturating_sub(2).max(4);
        let mut out: Vec<Line<'static>> = Vec::new();
        let mut owners: Vec<Option<usize>> = Vec::new();
        let indent = || vec![Span::raw("  ")];
        let mut previous: Option<&Entry> = None;
        for (index, entry) in self.entries.iter().enumerate() {
            // A reply's tool calls and text stay together under one heading.
            let continues = matches!(
                (previous, entry),
                (
                    Some(Entry::Tool { .. } | Entry::Assistant(_)),
                    Entry::Tool { .. } | Entry::Assistant(_)
                )
            );
            // Within a reply, a blank line where text and tool calls meet.
            let switches = continues
                && matches!(
                    (previous, entry),
                    (Some(Entry::Tool { .. }), Entry::Assistant(_))
                        | (Some(Entry::Assistant(_)), Entry::Tool { .. })
                );
            if !out.is_empty() && (!continues || switches) {
                out.push(Line::raw(""));
                owners.push(None);
            }
            let start = out.len();
            let heading = |out: &mut Vec<Line<'static>>| {
                out.push(Line::styled(
                    format!("{} {}", self.icon, self.name),
                    Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
                ));
            };
            match entry {
                Entry::User(text) => {
                    let bar = || vec![Span::styled("▌ ", Style::new().fg(USER))];
                    let mut lines = vec![Line::from(vec![
                        Span::styled("▌ ", Style::new().fg(USER)),
                        Span::styled(
                            YOU.get(self.lang),
                            Style::new().fg(USER).add_modifier(Modifier::BOLD),
                        ),
                    ])];
                    lines.extend(wrap_spans(
                        &[Span::raw(text.clone())],
                        width,
                        bar(),
                        bar(),
                        true,
                    ));
                    for line in lines {
                        // Filled to the edge so the message reads as one card.
                        let pad = width.saturating_sub(line.width());
                        let mut spans = line.spans;
                        spans.push(Span::raw(" ".repeat(pad)));
                        out.push(Line::from(spans).style(Style::new().bg(USER_BG)));
                    }
                }
                Entry::Assistant(text) => {
                    if !continues {
                        heading(&mut out);
                    }
                    for line in self.markdown(index, text, text_width) {
                        let mut spans = indent();
                        spans.extend(line.spans);
                        out.push(Line::from(spans));
                    }
                }
                Entry::Tool {
                    name,
                    detail,
                    output,
                    started,
                    elapsed,
                    expanded,
                } => {
                    if !continues {
                        heading(&mut out);
                    }
                    self.tool_lines(
                        &mut out, width, name, detail, output, *started, *elapsed, *expanded,
                    );
                }
                Entry::Note(text) => out.extend(wrap_spans(
                    &[Span::styled(text.clone(), muted())],
                    width,
                    indent(),
                    indent(),
                    true,
                )),
                Entry::Error(text) => out.extend(wrap_spans(
                    &[Span::styled(text.clone(), Style::new().fg(ERROR))],
                    width,
                    vec![Span::styled("✗ ", Style::new().fg(ERROR))],
                    indent(),
                    true,
                )),
            }
            let owner = matches!(entry, Entry::Tool { .. }).then_some(index);
            // A heading above a tool is not part of it.
            let heading_rows = usize::from(!continues && owner.is_some());
            owners.extend(
                (start..out.len()).enumerate().map(
                    |(k, _)| {
                        if k < heading_rows { None } else { owner }
                    },
                ),
            );
            previous = Some(entry);
        }
        (out, owners)
    }

    #[allow(clippy::too_many_arguments)]
    fn tool_lines(
        &self,
        out: &mut Vec<Line<'static>>,
        width: usize,
        name: &str,
        detail: &str,
        output: &Option<String>,
        started: std::time::Instant,
        elapsed: Option<Duration>,
        expanded: bool,
    ) {
        let failed = output
            .as_deref()
            .is_some_and(|o| o.trim_start().starts_with("error"));
        let (icon, status, color) = match (output, elapsed) {
            (None, _) => (
                SPINNER[self.spinner % SPINNER.len()].to_string(),
                seconds(started.elapsed()),
                WARN,
            ),
            (Some(_), elapsed) => {
                let (mark, color) = if failed {
                    ("✗", ERROR)
                } else {
                    ("✓", Color::Green)
                };
                let arrow = if expanded { "▾" } else { "▸" };
                (
                    arrow.to_owned(),
                    format!("{mark} {}", elapsed.map(seconds).unwrap_or_default()),
                    color,
                )
            }
        };
        let right = vec![Span::styled(format!(" {status}"), Style::new().fg(color))];
        let room = width.saturating_sub(6 + name.width() + status.width() + 2);
        let left = vec![
            Span::raw("  "),
            Span::styled(
                icon,
                Style::new().fg(if output.is_none() { WARN } else { MUTED }),
            ),
            Span::raw(" "),
            Span::styled(name.to_owned(), Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::styled(fit(detail, room), muted()),
        ];
        out.push(spread(left, right, width));
        let Some(output) = output.as_deref().filter(|_| expanded) else {
            return;
        };
        let gutter = || vec![Span::styled("    │ ", muted())];
        let text = output.trim_end();
        if text.is_empty() {
            out.push(Line::from(
                [
                    gutter(),
                    vec![Span::styled(NO_OUTPUT.get(self.lang), muted())],
                ]
                .concat(),
            ));
            return;
        }
        let mut rows = Vec::new();
        for line in text.lines() {
            rows.extend(wrap_spans(
                &[Span::styled(
                    line.replace('\t', "    "),
                    Style::new().fg(Color::Gray),
                )],
                width,
                gutter(),
                gutter(),
                false,
            ));
        }
        let more = rows.len().saturating_sub(TOOL_OUTPUT_LINES);
        out.extend(rows.into_iter().take(TOOL_OUTPUT_LINES));
        if more > 0 {
            out.push(Line::from(
                [
                    gutter(),
                    vec![Span::styled(
                        MORE_OUTPUT.fill(self.lang, &[&more.to_string()]),
                        muted().add_modifier(Modifier::ITALIC),
                    )],
                ]
                .concat(),
            ));
        }
    }
}
