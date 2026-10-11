//! `openclaw-rs tui`: a full-screen chat in the terminal.
//!
//! The agent runs in this process, as with `chat`, so it works with the
//! Gateway stopped or running without the Web UI (`gateway.web = "off"`).
//! Replies stream in, tool calls show as they run, approvals are a prompt
//! in the window, and messages typed while a turn runs are sent after it.

use std::collections::VecDeque;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use tokio::sync::{mpsc, oneshot};
use unicode_width::UnicodeWidthChar;

use crate::access::{self, Actor};
use crate::agent::{Agent, AgentEvent, Cancel, Model, Tools};
use crate::attachments::Upload;
use crate::i18n::{Lang, Tr, tr};
use crate::llm::Role;
use crate::tools::{Approver, with_approver};

const HINTS: Tr = tr(
    "Enter send · Alt+Enter new line · PgUp/PgDn scroll · /help · Ctrl+D quit",
    "回车发送 · Alt+回车换行 · PgUp/PgDn 滚动 · /help 帮助 · Ctrl+D 退出",
);
const WORKING: Tr = tr("working… Esc stops", "处理中… Esc 停止");
const STOPPING: Tr = tr("stopping…", "正在停止…");
const QUEUED: Tr = tr("{} waiting", "{} 条待发送");
const FILES: Tr = tr("files: {}", "附件：{}");
const YOU: Tr = tr("You", "你");
const HELP: Tr = tr(
    "Commands:\n  /attach <path>   send a file with the next message\n  /session <name>  switch to another session (created when new)\n  /sessions        list sessions\n  /new             start a fresh session\n  /clear           clear the screen (the history is kept)\n  /identity …      show or approve the identity, as in chat\n  /quit            leave\nKeys: Enter sends, Alt+Enter or Ctrl+J adds a line, ↑/↓ recall earlier messages, PgUp/PgDn or Shift+↑/↓ scroll, Esc or Ctrl+C stops a reply, Ctrl+D on an empty line quits.",
    "命令：\n  /attach <路径>   随下一条消息发送文件\n  /session <名字>  切换到另一个会话（不存在则新建）\n  /sessions        列出会话\n  /new             开始一个新会话\n  /clear           清屏（历史记录保留）\n  /identity …      查看或批准身份，和 chat 一样\n  /quit            退出\n按键：回车发送，Alt+回车或 Ctrl+J 换行，↑/↓ 调出之前的消息，PgUp/PgDn 或 Shift+↑/↓ 滚动，Esc 或 Ctrl+C 停止回复，空行时 Ctrl+D 退出。",
);
const SWITCHED: Tr = tr("Session: {}", "会话：{}");
const ATTACHED: Tr = tr(
    "Attached {}; it goes with the next message.",
    "已附加 {}，会随下一条消息发送。",
);
const NO_SESSIONS: Tr = tr("No sessions yet.", "还没有会话。");
const UNKNOWN: Tr = tr(
    "Unknown command {}; /help lists them.",
    "未知命令 {}；/help 查看所有命令。",
);
const ALLOW: Tr = tr("Allow {}?", "允许 {}？");
const ALLOW_KEYS: Tr = tr("y allow · n deny", "y 允许 · n 拒绝");
const DENIED: Tr = tr("Denied {}.", "已拒绝 {}。");
const QUIT_AGAIN: Tr = tr(
    "A reply is running: press Ctrl+D again to stop it and quit.",
    "回复还在进行：再按一次 Ctrl+D 停止并退出。",
);
const FIRST_START_NOTE: Tr = tr(
    "First start: the agent has no identity yet and will ask who it should be.",
    "首次启动：智能体还没有身份，会先问你它该是谁。",
);

#[cfg(test)]
const ALL: &[Tr] = &[
    HINTS,
    WORKING,
    STOPPING,
    QUEUED,
    FILES,
    YOU,
    HELP,
    SWITCHED,
    ATTACHED,
    NO_SESSIONS,
    UNKNOWN,
    ALLOW,
    ALLOW_KEYS,
    DENIED,
    QUIT_AGAIN,
    FIRST_START_NOTE,
];

const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// Lines of input shown before the box scrolls.
const MAX_INPUT_LINES: usize = 6;
const HISTORY_LOADED: usize = 200;

/// What the window shows, oldest first.
#[derive(Debug, Clone, PartialEq)]
enum Entry {
    User(String),
    Assistant(String),
    Tool {
        name: String,
        detail: String,
        done: Option<usize>,
    },
    Note(String),
    Error(String),
}

/// A request from a running turn.
enum UiEvent {
    Agent(AgentEvent),
    Approval {
        tool: String,
        summary: String,
        reply: oneshot::Sender<bool>,
    },
}

/// Approvals answered in the window.
struct TuiApprover(mpsc::UnboundedSender<UiEvent>);

#[async_trait]
impl Approver for TuiApprover {
    async fn approve(&self, tool: &str, summary: &str) -> bool {
        let (reply, answer) = oneshot::channel();
        let asked = self.0.send(UiEvent::Approval {
            tool: tool.to_owned(),
            summary: summary.to_owned(),
            reply,
        });
        asked.is_ok() && answer.await.unwrap_or(false)
    }
}

/// A message to send: typed text and the files attached before it.
struct Outgoing {
    text: String,
    files: Vec<Upload>,
}

/// What a key asked for, beyond editing the screen.
#[derive(Debug, PartialEq)]
enum Action {
    None,
    Send,
    Stop,
    Quit,
}

/// The line editor at the bottom; `cursor` is a char index.
#[derive(Default)]
struct Input {
    text: Vec<char>,
    cursor: usize,
}

impl Input {
    fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += 1;
    }

    fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text).into_iter().collect()
    }

    fn set(&mut self, text: &str) {
        self.text = text.chars().collect();
        self.cursor = self.text.len();
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.text.remove(self.cursor);
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.text.len() {
            self.text.remove(self.cursor);
        }
    }

    /// Deletes the word before the cursor (Ctrl+W).
    fn delete_word(&mut self) {
        let mut start = self.cursor;
        while start > 0 && self.text[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !self.text[start - 1].is_whitespace() {
            start -= 1;
        }
        self.text.drain(start..self.cursor);
        self.cursor = start;
    }

    /// The input broken into rows of `width` columns, and the cursor's row
    /// and column in them.
    fn layout(&self, width: usize) -> (Vec<String>, (usize, usize)) {
        let width = width.max(2);
        let mut rows = vec![String::new()];
        let mut used = 0;
        let mut cursor = (0, 0);
        for (i, &c) in self.text.iter().enumerate() {
            let w = if c == '\n' { 0 } else { c.width().unwrap_or(0) };
            if used + w > width {
                rows.push(String::new());
                used = 0;
            }
            if i == self.cursor {
                cursor = (rows.len() - 1, used);
            }
            if c == '\n' {
                rows.push(String::new());
                used = 0;
                continue;
            }
            rows.last_mut().expect("never empty").push(c);
            used += w;
        }
        if self.cursor >= self.text.len() {
            if used >= width {
                rows.push(String::new());
                used = 0;
            }
            cursor = (rows.len() - 1, used);
        }
        (rows, cursor)
    }
}

struct App {
    lang: Lang,
    name: String,
    model: String,
    session: String,
    entries: Vec<Entry>,
    input: Input,
    /// Messages sent before, for ↑/↓; `recall` is the one shown.
    sent: Vec<String>,
    recall: Option<usize>,
    files: Vec<Upload>,
    queue: VecDeque<Outgoing>,
    running: bool,
    stopping: bool,
    approval: Option<(String, String, oneshot::Sender<bool>)>,
    /// Lines scrolled up from the bottom.
    scroll: usize,
    spinner: usize,
    /// Ctrl+D was pressed once while a reply ran.
    quit_armed: bool,
    /// `/quit`: leave once the running reply, if any, has stopped.
    quit: bool,
}

impl App {
    fn new(lang: Lang, name: String, model: String, session: String) -> Self {
        Self {
            lang,
            name,
            model,
            session,
            entries: Vec::new(),
            input: Input::default(),
            sent: Vec::new(),
            recall: None,
            files: Vec::new(),
            queue: VecDeque::new(),
            running: false,
            stopping: false,
            approval: None,
            scroll: 0,
            spinner: 0,
            quit_armed: false,
            quit: false,
        }
    }

    fn note(&mut self, text: impl Into<String>) {
        self.entries.push(Entry::Note(text.into()));
        self.scroll = 0;
    }

    fn on_agent(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Text(delta) => match self.entries.last_mut() {
                Some(Entry::Assistant(text)) => text.push_str(&delta),
                _ => self.entries.push(Entry::Assistant(delta)),
            },
            AgentEvent::ToolStart { name, arguments } => self.entries.push(Entry::Tool {
                name,
                detail: one_line(&arguments, 200),
                done: None,
            }),
            AgentEvent::ToolEnd { name, output } => {
                let open =
                    self.entries.iter_mut().rev().find(
                        |e| matches!(e, Entry::Tool { name: n, done: None, .. } if *n == name),
                    );
                if let Some(Entry::Tool { done, .. }) = open {
                    *done = Some(output.len());
                }
            }
            AgentEvent::FollowUp(_) => {}
        }
    }

    /// The turn ended; `reply` is the whole answer already streamed.
    fn finished(&mut self, result: Result<String>) {
        self.running = false;
        self.stopping = false;
        match result {
            Ok(reply) => {
                // Replies that did not stream (stopped, a limit) show here.
                if !matches!(self.entries.last(), Some(Entry::Assistant(_)))
                    && !reply.trim().is_empty()
                {
                    self.entries.push(Entry::Assistant(reply));
                }
            }
            Err(err) => self.entries.push(Entry::Error(format!("{err:#}"))),
        }
    }

    fn key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if let Some((tool, _, _)) = &self.approval {
            let allow = match key.code {
                KeyCode::Char('y' | 'Y') => true,
                KeyCode::Char('n' | 'N') | KeyCode::Esc => false,
                KeyCode::Char('c') if ctrl => false,
                _ => return Action::None,
            };
            let tool = tool.clone();
            let (_, _, reply) = self.approval.take().expect("checked above");
            let _ = reply.send(allow);
            if !allow {
                self.note(DENIED.fill(self.lang, &[&tool]));
            }
            return Action::None;
        }
        if !matches!(key.code, KeyCode::Char('d')) || !ctrl {
            self.quit_armed = false;
        }
        match key.code {
            KeyCode::Enter if alt || shift => self.input.insert('\n'),
            KeyCode::Char('j') if ctrl => self.input.insert('\n'),
            KeyCode::Enter => {
                if self.input.text.iter().all(|c| c.is_whitespace()) && self.files.is_empty() {
                    return Action::None;
                }
                return Action::Send;
            }
            KeyCode::Esc if self.running => return Action::Stop,
            KeyCode::Char('c') if ctrl => {
                if self.running {
                    return Action::Stop;
                }
                if self.input.is_empty() {
                    return Action::Quit;
                }
                self.input.take();
            }
            KeyCode::Char('d') if ctrl => {
                if !self.input.is_empty() {
                    self.input.delete();
                } else if !self.running || self.quit_armed {
                    return Action::Quit;
                } else {
                    self.quit_armed = true;
                    let text = QUIT_AGAIN.get(self.lang);
                    self.note(text);
                }
            }
            KeyCode::Char('a') if ctrl => self.input.cursor = 0,
            KeyCode::Char('e') if ctrl => self.input.cursor = self.input.text.len(),
            KeyCode::Char('u') if ctrl => {
                self.input.text.drain(..self.input.cursor);
                self.input.cursor = 0;
            }
            KeyCode::Char('k') if ctrl => self.input.text.truncate(self.input.cursor),
            KeyCode::Char('w') if ctrl => self.input.delete_word(),
            KeyCode::Char('l') if ctrl => self.scroll = 0,
            KeyCode::Char(c) if !ctrl => self.input.insert(c),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.cursor = self.input.cursor.saturating_sub(1),
            KeyCode::Right => {
                self.input.cursor = (self.input.cursor + 1).min(self.input.text.len())
            }
            KeyCode::Home => self.input.cursor = 0,
            KeyCode::End => self.input.cursor = self.input.text.len(),
            KeyCode::PageUp => self.scroll += 10,
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Up if shift => self.scroll += 1,
            KeyCode::Down if shift => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Up => self.recall_older(),
            KeyCode::Down => self.recall_newer(),
            _ => {}
        }
        Action::None
    }

    fn recall_older(&mut self) {
        if self.sent.is_empty() || (self.recall.is_none() && !self.input.is_empty()) {
            return;
        }
        let i = match self.recall {
            None => self.sent.len() - 1,
            Some(i) => i.saturating_sub(1),
        };
        self.recall = Some(i);
        let text = self.sent[i].clone();
        self.input.set(&text);
    }

    fn recall_newer(&mut self) {
        let Some(i) = self.recall else { return };
        if i + 1 < self.sent.len() {
            self.recall = Some(i + 1);
            let text = self.sent[i + 1].clone();
            self.input.set(&text);
        } else {
            self.recall = None;
            self.input.take();
        }
    }

    /// Takes the typed text as a message, or runs it as a command. `Some`
    /// when there is something to send to the agent.
    fn submit<M: Model, T: Tools>(&mut self, agent: &Agent<M, T>) -> Option<Outgoing> {
        let text = self.input.take();
        self.recall = None;
        self.scroll = 0;
        let trimmed = text.trim();
        if !trimmed.is_empty() && self.sent.last().map(String::as_str) != Some(trimmed) {
            self.sent.push(trimmed.to_owned());
        }
        if trimmed.starts_with('/') && !trimmed.starts_with("/identity") {
            self.command(agent, trimmed);
            return None;
        }
        if trimmed.starts_with("/identity") {
            let owner = Actor::owner(access::CLI);
            if let Some(reply) = crate::identity::command(&agent.store, &owner, trimmed) {
                self.note(reply);
            }
            return None;
        }
        self.entries.push(Entry::User(text.trim_end().to_owned()));
        Some(Outgoing {
            text,
            files: std::mem::take(&mut self.files),
        })
    }

    fn command<M: Model, T: Tools>(&mut self, agent: &Agent<M, T>, line: &str) {
        let (name, arg) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let arg = arg.trim();
        match name {
            "/help" | "/?" => self.note(HELP.get(self.lang)),
            "/quit" | "/exit" | "/q" => self.quit = true,
            "/clear" => self.entries.clear(),
            "/attach" if !arg.is_empty() => match crate::read_upload(Path::new(arg)) {
                Ok(upload) => {
                    self.note(ATTACHED.fill(self.lang, &[&upload.name]));
                    self.files.push(upload);
                }
                Err(err) => self.entries.push(Entry::Error(format!("{err:#}"))),
            },
            "/session" if !arg.is_empty() => self.switch(agent, arg.to_owned()),
            "/new" => {
                let name = chrono::Local::now().format("tui-%Y%m%d-%H%M%S").to_string();
                self.switch(agent, name);
            }
            "/sessions" => match agent.store.sessions() {
                Ok(list) if list.is_empty() => self.note(NO_SESSIONS.get(self.lang)),
                Ok(list) => {
                    let rows: Vec<String> = list
                        .iter()
                        .map(|s| {
                            let mark = if s.name == self.session { "*" } else { " " };
                            format!("{mark} {}  ({})", s.name, s.messages)
                        })
                        .collect();
                    self.note(rows.join("\n"));
                }
                Err(err) => self.entries.push(Entry::Error(format!("{err:#}"))),
            },
            _ => self.note(UNKNOWN.fill(self.lang, &[name])),
        }
    }

    fn switch<M: Model, T: Tools>(&mut self, agent: &Agent<M, T>, session: String) {
        if crate::gateway::validate_session(&session).is_err() {
            self.note(UNKNOWN.fill(self.lang, &[&session]));
            return;
        }
        self.session = session;
        self.entries = load_history(agent, &self.session);
        self.note(SWITCHED.fill(self.lang, &[&self.session]));
    }

    fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        let input_width = usize::from(area.width.saturating_sub(4)).max(4);
        let (rows, cursor) = self.input.layout(input_width);
        let shown = rows.len().clamp(1, MAX_INPUT_LINES);
        let [header, body, status, input] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(shown as u16 + 2),
        ])
        .areas(area);

        let title = Line::from(vec![
            Span::styled(
                format!(" {} ", self.name),
                Style::new()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {} · {}", self.session, self.model),
                Style::new().fg(Color::DarkGray),
            ),
        ]);
        frame.render_widget(Paragraph::new(title), header);

        let width = usize::from(body.width.max(10));
        let lines = self.lines(width);
        let height = usize::from(body.height);
        let bottom = lines.len().saturating_sub(height);
        let top = bottom.saturating_sub(self.scroll.min(bottom));
        let visible: Vec<Line> = lines.into_iter().skip(top).take(height).collect();
        frame.render_widget(Paragraph::new(visible), body);

        let mut state = Vec::new();
        if self.running {
            let spin = SPINNER[self.spinner % SPINNER.len()];
            let text = if self.stopping { STOPPING } else { WORKING };
            state.push(Span::styled(
                format!(" {spin} {}", text.get(self.lang)),
                Style::new().fg(Color::Yellow),
            ));
        }
        if !self.queue.is_empty() {
            state.push(Span::styled(
                format!(
                    "  {}",
                    QUEUED.fill(self.lang, &[&self.queue.len().to_string()])
                ),
                Style::new().fg(Color::Magenta),
            ));
        }
        if !self.files.is_empty() {
            let names: Vec<&str> = self.files.iter().map(|f| f.name.as_str()).collect();
            state.push(Span::styled(
                format!("  {}", FILES.fill(self.lang, &[&names.join(", ")])),
                Style::new().fg(Color::Blue),
            ));
        }
        if self.scroll > 0 {
            state.push(Span::styled(
                format!("  ↑{}", self.scroll),
                Style::new().fg(Color::DarkGray),
            ));
        }
        if state.is_empty() {
            state.push(Span::styled(
                format!(" {}", HINTS.get(self.lang)),
                Style::new().fg(Color::DarkGray),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(state)), status);

        let first = (cursor.0 + 1).saturating_sub(shown);
        let text: Vec<Line> = rows
            .iter()
            .skip(first)
            .take(shown)
            .map(|r| Line::raw(r.clone()))
            .collect();
        let border = if self.running {
            Color::DarkGray
        } else {
            Color::Cyan
        };
        let block = Block::bordered()
            .border_style(Style::new().fg(border))
            .title(format!(" {} ", self.session));
        frame.render_widget(Paragraph::new(text).block(block), input);
        if self.approval.is_none() {
            frame.set_cursor_position(Position::new(
                input.x + 2 + cursor.1 as u16,
                input.y + 1 + (cursor.0 - first) as u16,
            ));
        }

        if let Some((tool, summary, _)) = &self.approval {
            self.draw_approval(frame, area, tool, summary);
        }
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
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
        let height = (lines.len() as u16 + 2).min(area.height);
        let popup = Rect::new(
            area.x + (area.width.saturating_sub(width)) / 2,
            area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
        );
        let block = Block::bordered()
            .border_style(Style::new().fg(Color::Yellow))
            .title(format!(" {} ", ALLOW.fill(self.lang, &[tool])));
        frame.render_widget(Clear, popup);
        frame.render_widget(Paragraph::new(lines).block(block), popup);
    }

    /// Every entry as screen lines `width` columns wide.
    fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let text_width = width.saturating_sub(2).max(4);
        let mut out = Vec::new();
        let label = |text: &str, color: Color| {
            Line::styled(
                text.to_owned(),
                Style::new().fg(color).add_modifier(Modifier::BOLD),
            )
        };
        let body = |out: &mut Vec<Line<'static>>, text: &str, style: Style| {
            for row in wrap(text, text_width) {
                out.push(Line::styled(format!("  {row}"), style));
            }
        };
        let mut previous: Option<&Entry> = None;
        for entry in &self.entries {
            // A reply's tool calls and text stay together under one label.
            let continues = matches!(
                (previous, entry),
                (
                    Some(Entry::Tool { .. } | Entry::Assistant(_)),
                    Entry::Tool { .. } | Entry::Assistant(_)
                )
            );
            if !out.is_empty() && !continues {
                out.push(Line::raw(""));
            }
            match entry {
                Entry::User(text) => {
                    out.push(label(YOU.get(self.lang), Color::Green));
                    body(&mut out, text, Style::new());
                }
                Entry::Assistant(text) => {
                    if !continues {
                        out.push(label(&self.name, Color::Cyan));
                    }
                    body(&mut out, text, Style::new());
                }
                Entry::Tool { name, detail, done } => {
                    if !continues {
                        out.push(label(&self.name, Color::Cyan));
                    }
                    let (mark, color) = match done {
                        Some(_) => ("✓", Color::DarkGray),
                        None => ("⚙", Color::Yellow),
                    };
                    let row = one_line(&format!("{mark} {name} {detail}"), text_width);
                    out.push(Line::styled(format!("  {row}"), Style::new().fg(color)));
                }
                Entry::Note(text) => body(&mut out, text, Style::new().fg(Color::DarkGray)),
                Entry::Error(text) => body(&mut out, text, Style::new().fg(Color::Red)),
            }
            previous = Some(entry);
        }
        out
    }
}

/// `text` on one line, cut to `max` columns.
fn one_line(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    let mut used = 0;
    for c in flat.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > max {
            out.push('…');
            return out;
        }
        out.push(c);
        used += w;
    }
    out
}

/// `text` broken into rows of at most `width` columns, at spaces when it can.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(2);
    let mut rows = Vec::new();
    for paragraph in text.split('\n') {
        let mut row = String::new();
        let mut used = 0;
        for c in paragraph.chars() {
            let c = if c == '\t' { ' ' } else { c };
            let w = c.width().unwrap_or(0);
            if used + w > width {
                match row.rfind(' ').filter(|&i| i > 0) {
                    Some(i) => {
                        let rest = row.split_off(i + 1);
                        rows.push(row.trim_end().to_owned());
                        row = rest;
                    }
                    None => rows.push(std::mem::take(&mut row)),
                }
                used = row.chars().map(|c| c.width().unwrap_or(0)).sum();
            }
            row.push(c);
            used += w;
        }
        rows.push(row);
    }
    rows
}

/// The session's earlier messages, as people see them.
fn load_history<M: Model, T: Tools>(agent: &Agent<M, T>, session: &str) -> Vec<Entry> {
    let Ok(id) = agent.store.session_id(session) else {
        return Vec::new();
    };
    let Ok(messages) = agent.store.history(id, HISTORY_LOADED) else {
        return Vec::new();
    };
    messages
        .into_iter()
        .filter_map(|m| {
            let mut text = crate::agent::for_people(m.content.as_deref().unwrap_or(""));
            if !m.attachments.is_empty() {
                let names: Vec<&str> = m.attachments.iter().map(|a| a.name.as_str()).collect();
                text = format!("{text}\n📎 {}", names.join(", ")).trim().to_owned();
            }
            if text.is_empty() {
                return None;
            }
            match m.role {
                Role::User => Some(Entry::User(text)),
                Role::Assistant => Some(Entry::Assistant(text)),
                Role::Tool | Role::System => None,
            }
        })
        .collect()
}

type Turn<'a> = Pin<Box<dyn Future<Output = Result<String>> + 'a>>;

fn start_turn<'a, M: Model, T: Tools>(
    agent: &'a Agent<M, T>,
    session: String,
    message: Outgoing,
    events: mpsc::UnboundedSender<UiEvent>,
    cancel: Cancel,
) -> Turn<'a> {
    Box::pin(async move {
        let actor = Actor::owner(access::CLI);
        let (files, text) = agent
            .receive(&actor, &session, &message.text, &message.files)
            .await;
        let agent_events = events.clone();
        let mut on_event = move |event| {
            let _ = agent_events.send(UiEvent::Agent(event));
        };
        let approver: Arc<dyn Approver> = Arc::new(TuiApprover(events));
        with_approver(
            approver,
            agent.run_turn_until(actor, &session, &text, &files, &cancel, &mut on_event),
        )
        .await
    })
}

async fn wait(turn: &mut Option<Turn<'_>>) -> Result<String> {
    match turn {
        Some(turn) => turn.await,
        None => std::future::pending().await,
    }
}

/// Sends stray stderr output (logs from tools, MCP servers) to a file while
/// the window is up, so it cannot scribble over the screen.
struct StderrToFile(Option<i32>);

impl StderrToFile {
    fn new(path: &Path) -> Self {
        use std::os::fd::IntoRawFd;
        let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        else {
            return Self(None);
        };
        let fd = file.into_raw_fd();
        // SAFETY: dup and dup2 on valid descriptors; the copy is restored in drop.
        unsafe {
            let saved = libc::dup(libc::STDERR_FILENO);
            libc::dup2(fd, libc::STDERR_FILENO);
            libc::close(fd);
            Self((saved >= 0).then_some(saved))
        }
    }
}

impl Drop for StderrToFile {
    fn drop(&mut self) {
        if let Some(saved) = self.0 {
            // SAFETY: puts back the descriptor saved in `new`.
            unsafe {
                libc::dup2(saved, libc::STDERR_FILENO);
                libc::close(saved);
            }
        }
    }
}

/// Puts the terminal back however the window ends.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

enum Step {
    Key(KeyEvent),
    Redraw,
    Ui(UiEvent),
    Done(Result<String>),
    Tick,
    End,
}

/// Runs the window until the person quits.
pub async fn run<M: Model, T: Tools>(
    agent: &Agent<M, T>,
    session: String,
    model: String,
    state: &Path,
) -> Result<()> {
    let lang = crate::i18n::current();
    let identity = agent.store.identity()?;
    let name = identity.as_ref().map_or_else(
        || "OpenClaw".to_owned(),
        |i| match &i.emoji {
            Some(emoji) => format!("{emoji} {}", i.name),
            None => i.name.clone(),
        },
    );
    let mut app = App::new(lang, name, model, session);
    app.entries = load_history(agent, &app.session);
    if identity.is_none() {
        app.note(FIRST_START_NOTE.get(lang));
    }

    let _stderr = StderrToFile::new(&state.join("tui.log"));
    let mut terminal = ratatui::init();
    let _restore = Restore;
    let mut keys = EventStream::new();
    let (events, mut inbox) = mpsc::unbounded_channel();
    let mut turn: Option<Turn> = None;
    let mut cancel = Cancel::default();
    let mut tick = tokio::time::interval(Duration::from_millis(100));

    loop {
        terminal.draw(|frame| app.draw(frame))?;
        let step = tokio::select! {
            // Agent events before the turn's end, so no delta comes after it.
            biased;
            Some(event) = inbox.recv() => Step::Ui(event),
            event = keys.next() => match event {
                Some(Ok(Event::Key(key))) => Step::Key(key),
                Some(Ok(Event::Paste(text))) => {
                    for c in text.chars() {
                        app.input.insert(if c == '\r' { '\n' } else { c });
                    }
                    Step::Redraw
                }
                Some(Ok(_)) => Step::Redraw,
                Some(Err(err)) => return Err(err.into()),
                None => Step::End,
            },
            result = wait(&mut turn) => Step::Done(result),
            _ = tick.tick(), if app.running => Step::Tick,
        };
        match step {
            Step::Key(key) => match app.key(key) {
                Action::None => {}
                Action::Send => {
                    if let Some(message) = app.submit(agent) {
                        if app.running {
                            app.queue.push_back(message);
                        } else {
                            cancel = Cancel::default();
                            turn = Some(start_turn(
                                agent,
                                app.session.clone(),
                                message,
                                events.clone(),
                                cancel.clone(),
                            ));
                            app.running = true;
                        }
                    }
                }
                Action::Stop => {
                    // Stopping also drops what was waiting to go after it.
                    app.queue.clear();
                    app.stopping = true;
                    cancel.cancel();
                }
                Action::Quit => break,
            },
            Step::Redraw => {}
            Step::Ui(UiEvent::Agent(event)) => app.on_agent(event),
            Step::Ui(UiEvent::Approval {
                tool,
                summary,
                reply,
            }) => app.approval = Some((tool, summary, reply)),
            Step::Done(result) => {
                turn = None;
                // The last deltas may still be on their way.
                while let Ok(event) = inbox.try_recv() {
                    if let UiEvent::Agent(event) = event {
                        app.on_agent(event);
                    }
                }
                app.finished(result);
                if let Some(message) = app.queue.pop_front() {
                    cancel = Cancel::default();
                    turn = Some(start_turn(
                        agent,
                        app.session.clone(),
                        message,
                        events.clone(),
                        cancel.clone(),
                    ));
                    app.running = true;
                }
            }
            Step::Tick => app.spinner += 1,
            Step::End => break,
        }
        if app.quit {
            break;
        }
    }
    if let Some(running) = turn.take() {
        // Let a stopped turn save what it has before leaving.
        cancel.cancel();
        drop(terminal);
        let _ = running.await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Action {
        app.key(KeyEvent::new(code, modifiers))
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c), KeyModifiers::NONE);
        }
    }

    fn app() -> App {
        App::new(Lang::En, "Claw".into(), "x/y".into(), "main".into())
    }

    #[test]
    fn edits_the_input_line() {
        let mut app = app();
        typed(&mut app, "hello world");
        press(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
        typed(&mut app, "there");
        press(&mut app, KeyCode::Home, KeyModifiers::NONE);
        press(&mut app, KeyCode::Delete, KeyModifiers::NONE);
        typed(&mut app, "H");
        press(&mut app, KeyCode::End, KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::ALT);
        typed(&mut app, "x");
        let text: String = app.input.text.iter().collect();
        assert_eq!(text, "Hello there\nx");
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        let text: String = app.input.text.iter().collect();
        assert_eq!(text, "Hello therex");
        assert_eq!(
            press(&mut app, KeyCode::Enter, KeyModifiers::NONE),
            Action::Send
        );
    }

    #[test]
    fn keys_stop_a_reply_and_quit_when_idle() {
        let mut app = app();
        assert_eq!(
            press(&mut app, KeyCode::Enter, KeyModifiers::NONE),
            Action::None
        );
        assert_eq!(
            press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL),
            Action::Quit
        );
        app.running = true;
        assert_eq!(
            press(&mut app, KeyCode::Esc, KeyModifiers::NONE),
            Action::Stop
        );
        assert_eq!(
            press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL),
            Action::Stop
        );
        // Ctrl+D asks again while a reply runs.
        assert_eq!(
            press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL),
            Action::None
        );
        assert_eq!(
            press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL),
            Action::Quit
        );
    }

    #[test]
    fn approvals_take_y_or_n() {
        let mut app = app();
        let (reply, answer) = oneshot::channel();
        app.approval = Some(("shell".into(), "rm -rf build".into(), reply));
        typed(&mut app, "x");
        assert!(app.approval.is_some(), "other keys wait");
        typed(&mut app, "y");
        assert!(app.approval.is_none());
        assert_eq!(answer.blocking_recv(), Ok(true));
        // The tool line shows it ran; only a refusal is noted.
        assert!(app.entries.is_empty());
        let (reply, answer) = oneshot::channel();
        app.approval = Some(("shell".into(), "rm -rf build".into(), reply));
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(answer.blocking_recv(), Ok(false));
        assert_eq!(
            app.entries.last(),
            Some(&Entry::Note("Denied shell.".into()))
        );
    }

    #[test]
    fn recalls_earlier_messages() {
        let mut app = app();
        app.sent = vec!["one".into(), "two".into()];
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.input.text.iter().collect::<String>(), "one");
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.input.text.iter().collect::<String>(), "two");
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert!(app.input.is_empty());
    }

    #[test]
    fn streams_text_and_marks_tools_done() {
        let mut app = app();
        app.on_agent(AgentEvent::ToolStart {
            name: "shell".into(),
            arguments: "{\"command\":\n\"ls\"}".into(),
        });
        app.on_agent(AgentEvent::ToolEnd {
            name: "shell".into(),
            output: "a b".into(),
        });
        app.on_agent(AgentEvent::Text("Hel".into()));
        app.on_agent(AgentEvent::Text("lo".into()));
        app.finished(Ok("Hello".into()));
        assert_eq!(
            app.entries,
            [
                Entry::Tool {
                    name: "shell".into(),
                    detail: "{\"command\": \"ls\"}".into(),
                    done: Some(3)
                },
                Entry::Assistant("Hello".into())
            ]
        );
        // A reply that did not stream still shows.
        app.entries.push(Entry::User("again".into()));
        app.finished(Ok("Stopped.".into()));
        assert_eq!(
            app.entries.last(),
            Some(&Entry::Assistant("Stopped.".into()))
        );
        let lines: Vec<String> = app
            .lines(40)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(lines[0], "Claw");
        assert!(lines[1].starts_with("  ✓ shell"), "{lines:?}");
        assert_eq!(lines[2], "  Hello");
    }

    #[test]
    fn lays_out_wide_text_and_the_cursor() {
        assert_eq!(wrap("one two three", 8), ["one two", "three"]);
        assert_eq!(wrap("中文中文中文", 4), ["中文", "中文", "中文"]);
        assert_eq!(wrap("a\n\nb", 10), ["a", "", "b"]);
        assert_eq!(one_line("a\n  b   c", 10), "a b c");
        assert_eq!(one_line("abcdefgh", 5), "abcd…");
        let mut input = Input::default();
        input.set("中文ab\ncd");
        let (rows, cursor) = input.layout(4);
        assert_eq!(rows, ["中文", "ab", "cd"]);
        assert_eq!(cursor, (2, 2));
        input.cursor = 2;
        assert_eq!(input.layout(4).1, (1, 0));
    }

    #[test]
    fn every_text_has_both_languages() {
        crate::i18n::assert_complete(ALL);
    }
}
