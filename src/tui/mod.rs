//! `runo tui`: a full-screen chat in the terminal.
//!
//! The agent runs in this process, as with `chat`, so it works with the
//! Gateway stopped or running without the Web UI (`gateway.web = "off"`).
//! Sessions are listed in a sidebar, replies render as Markdown and stream
//! in, tool calls show as they run and open to show their output, approvals
//! are a prompt in the window, and messages typed while a turn runs are sent
//! after it. `/` and Ctrl+K open the commands; the mouse scrolls and clicks.
//! `!shell` leaves the window for a shell and comes back when it exits; it
//! needs no model, so it works when the model is down.

mod backend;
mod highlight;
mod markdown;
mod view;

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use futures_util::StreamExt;
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;
use tokio::sync::{mpsc, oneshot};
use unicode_width::UnicodeWidthChar;

use crate::access::{self, Actor};
use crate::agent::{Agent, AgentEvent, Cancel, Model, Tools};
use crate::attachments::Upload;
use crate::i18n::{Lang, Tr, tr};
use crate::llm::Role;
use crate::skills::Skills;
use crate::store::SessionSummary;
use crate::tools::{Approver, with_approver};

const HINTS: Tr = tr(
    "Enter send · Alt+Enter new line · ^K commands · ^N new · ^B sessions · ^O tool output",
    "回车发送 · Alt+回车换行 · ^K 命令 · ^N 新会话 · ^B 会话栏 · ^O 工具输出",
);
const SIDEBAR_HINTS: Tr = tr(
    "↑/↓ choose · Enter open · ^N new · Esc back",
    "↑/↓ 选择 · 回车打开 · ^N 新会话 · Esc 返回",
);
const WORKING: Tr = tr("working… Esc stops", "处理中… Esc 停止");
const STOPPING: Tr = tr("stopping…", "正在停止…");
const QUEUED: Tr = tr("{} waiting", "{} 条待发送");
const FILES: Tr = tr("files: {}", "附件：{}");
const YOU: Tr = tr("You", "你");
const HELP: Tr = tr(
    "Commands (type / or press Ctrl+K for the list):\n  /attach <path>   send a file with the next message\n  /session <name>  switch to another session (created when new)\n  /sessions        list sessions\n  /new             start a fresh session\n  /clear           clear the screen (the history is kept)\n  /skills          list skills; @name in a message asks the agent to use one\n  /compact         fold the conversation into a summary to free context\n  /identity …      show or approve the identity, as in chat\n  /sidebar         show or hide the session list\n  /expand          open or close every tool output\n  /config          edit the settings: heartbeat, inner life, QQ, models…\n  /quit            leave\n  !shell           open a shell; exit returns here (works without a model)\nKeys: Enter sends, Alt+Enter or Ctrl+J adds a line, ↑/↓ recall earlier messages, PgUp/PgDn, Shift+↑/↓ or the mouse wheel scroll, Esc or Ctrl+C stops a reply, Ctrl+D on an empty line quits.\nCtrl+K commands and sessions · Ctrl+N new session · Ctrl+B session list · Tab moves between the list and the input · Ctrl+O opens tool output, or click a tool line · Ctrl+L repaints the screen. Hold Shift to select text with the mouse.",
    "命令（输入 / 或按 Ctrl+K 弹出列表）：\n  /attach <路径>   随下一条消息发送文件\n  /session <名字>  切换到另一个会话（不存在则新建）\n  /sessions        列出会话\n  /new             开始一个新会话\n  /clear           清屏（历史记录保留）\n  /skills          列出 skills；消息里写 @名字 让智能体使用它\n  /compact         把对话折叠成摘要，腾出上下文\n  /identity …      查看或批准身份，和 chat 一样\n  /sidebar         显示或隐藏会话栏\n  /expand          展开或收起所有工具输出\n  /config          修改设置：心跳、内心生活、QQ、模型……\n  /quit            退出\n  !shell           打开 Shell，exit 后回到这里（不需要模型也能用）\n按键：回车发送，Alt+回车或 Ctrl+J 换行，↑/↓ 调出之前的消息，PgUp/PgDn、Shift+↑/↓ 或鼠标滚轮滚动，Esc 或 Ctrl+C 停止回复，空行时 Ctrl+D 退出。\nCtrl+K 命令和会话 · Ctrl+N 新会话 · Ctrl+B 会话栏 · Tab 在会话栏和输入框之间切换 · Ctrl+O 展开工具输出，也可以点击工具那一行 · Ctrl+L 重绘屏幕。按住 Shift 用鼠标选中文字。",
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
const CONTEXT: Tr = tr("context {} / {}", "上下文 {} / {}");
const SKILLS_OFF: Tr = tr(
    "Skills are off (skills.enabled in runo config).",
    "Skills 已关闭（在 runo config 里打开 skills.enabled）。",
);
const NO_SKILLS: Tr = tr(
    "No skills yet: runo skills install <source>, or ask the agent to save one.",
    "还没有 skill：用 runo skills install <来源> 安装，或者让智能体保存一个。",
);
const NO_MATCH: Tr = tr("nothing matches", "没有匹配项");
const FIRST_START_NOTE: Tr = tr(
    "First start: the agent has no identity yet and will ask who it should be.",
    "首次启动：智能体还没有身份，会先问你它该是谁。",
);
const SESSIONS: Tr = tr("Sessions", "会话");
const PLACEHOLDER: Tr = tr(
    "Message {} · / commands · @ skills",
    "给 {} 发消息 · / 命令 · @ skills",
);
const PALETTE: Tr = tr("Commands", "命令");
const PALETTE_SESSION: Tr = tr("open session", "打开会话");
const MORE_OUTPUT: Tr = tr(
    "… {} more lines (Ctrl+O closes)",
    "… 还有 {} 行（Ctrl+O 收起）",
);
const NO_OUTPUT: Tr = tr("(no output)", "（没有输出）");

const CMD_HELP: Tr = tr("commands and keys", "命令和按键");
const CMD_NEW: Tr = tr("start a fresh session", "开始一个新会话");
const CMD_SESSION: Tr = tr("switch to a session by name", "按名字切换会话");
const CMD_SESSIONS: Tr = tr("list sessions", "列出会话");
const CMD_ATTACH: Tr = tr("send a file with the next message", "随下一条消息发送文件");
const CMD_CLEAR: Tr = tr("clear the screen", "清屏");
const CMD_SKILLS: Tr = tr("list skills", "列出 skills");
const CMD_COMPACT: Tr = tr("fold the conversation into a summary", "把对话折叠成摘要");
const CMD_IDENTITY: Tr = tr("show or approve the identity", "查看或批准身份");
const CMD_SIDEBAR: Tr = tr("show or hide the session list", "显示或隐藏会话栏");
const CMD_EXPAND: Tr = tr("open or close every tool output", "展开或收起所有工具输出");
const CMD_CONFIG: Tr = tr(
    "edit the settings: heartbeat, inner life, QQ, models…",
    "修改设置：心跳、内心生活、QQ、模型……",
);
const CONFIG_BUSY: Tr = tr(
    "Wait for the reply to finish, then open /config.",
    "等这次回复结束后再打开 /config。",
);
const CONFIG_APPLIED: Tr = tr(
    "Settings saved, and the Gateway was restarted to use them. This window uses them once you open it again (/quit, then runo tui).",
    "设置已保存，Gateway 已重启并用上了新设置。这个窗口重新打开后才会用上（/quit 后再运行 runo tui）。",
);
const CONFIG_SAVED: Tr = tr(
    "Settings saved. Restart the Gateway (runo service restart) and open this window again to use them.",
    "设置已保存。重启 Gateway（runo service restart）并重新打开这个窗口后生效。",
);
const CMD_QUIT: Tr = tr("leave", "退出");
const SHELL_BANNER: Tr = tr(
    "Runo shell ({}). Type exit to go back to the agent.",
    "Runo Shell（{}）。输入 exit 回到智能体。",
);
const SHELL_BACK: Tr = tr("Back from the shell ({}).", "已从 Shell 返回（{}）。");
const SHELL_FAILED: Tr = tr("Could not start {}: {}", "无法启动 {}：{}");

#[cfg(test)]
const ALL: &[Tr] = &[
    HINTS,
    SIDEBAR_HINTS,
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
    CONTEXT,
    SKILLS_OFF,
    NO_SKILLS,
    NO_MATCH,
    SESSIONS,
    PLACEHOLDER,
    PALETTE,
    PALETTE_SESSION,
    MORE_OUTPUT,
    NO_OUTPUT,
    CMD_HELP,
    CMD_NEW,
    CMD_SESSION,
    CMD_SESSIONS,
    CMD_ATTACH,
    CMD_CLEAR,
    CMD_SKILLS,
    CMD_COMPACT,
    CMD_IDENTITY,
    CMD_SIDEBAR,
    CMD_EXPAND,
    CMD_CONFIG,
    CONFIG_BUSY,
    CONFIG_APPLIED,
    CONFIG_SAVED,
    CMD_QUIT,
    SHELL_BANNER,
    SHELL_BACK,
    SHELL_FAILED,
];

/// What opens a shell from the input line.
const SHELL_COMMAND: &str = "!shell";

/// Whether a line typed in the input asks for the shell.
fn is_shell_command(text: &str) -> bool {
    text.trim() == SHELL_COMMAND
}

/// A slash command: its name, whether it needs an argument, what it does.
struct Command {
    name: &'static str,
    arg: bool,
    help: Tr,
}

const COMMANDS: &[Command] = &[
    Command {
        name: "new",
        arg: false,
        help: CMD_NEW,
    },
    Command {
        name: "session",
        arg: true,
        help: CMD_SESSION,
    },
    Command {
        name: "sessions",
        arg: false,
        help: CMD_SESSIONS,
    },
    Command {
        name: "attach",
        arg: true,
        help: CMD_ATTACH,
    },
    Command {
        name: "skills",
        arg: false,
        help: CMD_SKILLS,
    },
    Command {
        name: "compact",
        arg: false,
        help: CMD_COMPACT,
    },
    Command {
        name: "identity",
        arg: false,
        help: CMD_IDENTITY,
    },
    Command {
        name: "clear",
        arg: false,
        help: CMD_CLEAR,
    },
    Command {
        name: "sidebar",
        arg: false,
        help: CMD_SIDEBAR,
    },
    Command {
        name: "expand",
        arg: false,
        help: CMD_EXPAND,
    },
    Command {
        name: "config",
        arg: false,
        help: CMD_CONFIG,
    },
    Command {
        name: "help",
        arg: false,
        help: CMD_HELP,
    },
    Command {
        name: "quit",
        arg: false,
        help: CMD_QUIT,
    },
];

/// Added to a message that names skills with @, for the model.
const SKILL_HINT: &str = "(Skills to use: {}. Load each with the skill tool before you start.)";
/// Rows shown at once in a list popup.
const MAX_SUGGESTIONS: usize = 8;
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// Lines of input shown before the box scrolls.
const MAX_INPUT_LINES: usize = 6;
const HISTORY_LOADED: usize = 200;
/// Tool output kept for showing; the model got all of it.
const MAX_TOOL_OUTPUT: usize = 64 * 1024;
/// Lines of tool output shown when it is opened.
const TOOL_OUTPUT_LINES: usize = 40;
/// Below this width the session list hides itself.
const SIDEBAR_MIN_WIDTH: u16 = 72;
/// Lines one mouse wheel step scrolls.
const WHEEL_LINES: usize = 3;

/// What a list popup completes.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    /// `@skill` anywhere in the input.
    Skill,
    /// `/command` at the start of the input.
    Command,
}

/// A popup list under the cursor: skills after @, commands after a leading /.
#[derive(Debug)]
struct Completion {
    kind: Kind,
    /// Index of the @ or / in the input.
    start: usize,
    /// Name and description; an empty name is a note, not a choice.
    items: Vec<(String, String)>,
    selected: usize,
}

/// One row of the Ctrl+K palette.
#[derive(Debug, Clone, PartialEq)]
struct PaletteItem {
    label: String,
    detail: String,
    /// The command line it runs.
    line: String,
    /// Put the line in the input to finish typing instead of running it.
    edit: bool,
}

/// The Ctrl+K palette: commands and sessions, filtered by what is typed.
#[derive(Default)]
struct Palette {
    query: Input,
    selected: usize,
}

/// What the window shows, oldest first.
#[derive(Debug, Clone, PartialEq)]
enum Entry {
    User(String),
    Assistant(String),
    Tool {
        name: String,
        detail: String,
        /// `None` while it runs.
        output: Option<String>,
        started: Instant,
        elapsed: Option<Duration>,
        expanded: bool,
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

/// What a key or click asked for, beyond editing the screen.
#[derive(Debug, PartialEq)]
enum Action {
    None,
    /// Send what is in the input.
    Send,
    /// Run this command line, leaving the input as it is.
    Run(String),
    Stop,
    Quit,
}

/// Where keys go.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Focus {
    Input,
    Sidebar,
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

    fn string(&self) -> String {
        self.text.iter().collect()
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

    /// Keys every one-line editor shares; `false` when the key is not one.
    fn edit_key(&mut self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.text.len(),
            KeyCode::Char('u') if ctrl => {
                self.text.drain(..self.cursor);
                self.cursor = 0;
            }
            KeyCode::Char('w') if ctrl => self.delete_word(),
            KeyCode::Char(c) if !ctrl => self.insert(c),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.text.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.len(),
            _ => return false,
        }
        true
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

/// Where things were drawn, for the mouse.
#[derive(Default)]
struct Hits {
    body: Rect,
    /// The entry each visible body row belongs to, for tool lines.
    body_rows: Vec<Option<usize>>,
    sidebar: Rect,
    /// The session on each visible sidebar row, from its first list row.
    sidebar_rows: Vec<String>,
    sidebar_first_row: u16,
}

/// A message's rendered lines, kept while its text and the width stay the same.
struct Cached {
    width: usize,
    hash: u64,
    lines: Vec<Line<'static>>,
}

struct App {
    lang: Lang,
    name: String,
    /// The identity's emoji, or ✻.
    icon: String,
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
    /// Ctrl+L or `/clear`: the next draw repaints the whole screen.
    repaint: bool,
    /// `/config`: the settings editor opens before the next draw.
    open_config: bool,
    /// `!shell`: leave the window for a shell before the next draw.
    open_shell: bool,
    /// Ctrl+D was pressed once while a reply ran.
    quit_armed: bool,
    /// `/quit`: leave once the running reply, if any, has stopped.
    quit: bool,
    /// `None` when skills are off.
    skills: Option<Skills>,
    /// Ready skills (name, description), read when an @ list opens.
    known_skills: Vec<(String, String)>,
    completion: Option<Completion>,
    /// The @ or / whose list was closed with Esc; it stays closed.
    dismissed: Option<usize>,
    /// Estimated tokens of the next call, and the budget.
    context: Option<(usize, usize)>,
    sessions: Vec<SessionSummary>,
    /// The person wants the session list (it still hides when narrow).
    sidebar: bool,
    focus: Focus,
    /// Highlighted row of the session list while it has focus.
    sidebar_selected: usize,
    palette: Option<Palette>,
    hits: RefCell<Hits>,
    cache: RefCell<HashMap<usize, Cached>>,
}

impl App {
    fn new(lang: Lang, name: String, model: String, session: String) -> Self {
        Self {
            lang,
            name,
            icon: "✻".into(),
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
            repaint: false,
            open_config: false,
            open_shell: false,
            quit_armed: false,
            quit: false,
            skills: None,
            known_skills: Vec::new(),
            completion: None,
            dismissed: None,
            context: None,
            sessions: Vec::new(),
            sidebar: true,
            focus: Focus::Input,
            sidebar_selected: 0,
            palette: None,
            hits: RefCell::default(),
            cache: RefCell::default(),
        }
    }

    /// The `@word` the cursor is in or right after: where its @ is, and the
    /// text typed after it.
    fn mention_at_cursor(&self) -> Option<(usize, String)> {
        let text = &self.input.text;
        let mut start = self.input.cursor;
        while start > 0 && !text[start - 1].is_whitespace() {
            start -= 1;
        }
        (text.get(start) == Some(&'@'))
            .then(|| (start, text[start + 1..self.input.cursor].iter().collect()))
    }

    /// The command name being typed: the input starts with / and the cursor
    /// is still in its first word.
    fn command_at_cursor(&self) -> Option<String> {
        let text = &self.input.text;
        let before = &text[..self.input.cursor];
        (text.first() == Some(&'/') && !before.iter().any(|c| c.is_whitespace()))
            .then(|| before[1..].iter().collect())
    }

    /// Opens, narrows or closes the popup list after the input changed.
    fn refresh_completion(&mut self) {
        let (kind, start, query) = if let Some(query) = self.command_at_cursor() {
            (Kind::Command, 0, query)
        } else if let Some((start, query)) = self.mention_at_cursor() {
            (Kind::Skill, start, query)
        } else {
            self.completion = None;
            self.dismissed = None;
            return;
        };
        if self.dismissed == Some(start) {
            return;
        }
        let reopened = self
            .completion
            .as_ref()
            .is_none_or(|c| c.start != start || c.kind != kind);
        let mut items: Vec<(String, String)> = match kind {
            Kind::Command => {
                let all: Vec<(String, String)> = COMMANDS
                    .iter()
                    .map(|c| (c.name.to_owned(), c.help.get(self.lang).to_owned()))
                    .collect();
                ranked(&all, &query, |(name, _)| name)
                    .into_iter()
                    .cloned()
                    .collect()
            }
            Kind::Skill => {
                if reopened {
                    self.known_skills = self
                        .skills
                        .as_ref()
                        .map(|s| {
                            s.ready()
                                .into_iter()
                                .map(|s| (s.name, s.description))
                                .collect()
                        })
                        .unwrap_or_default();
                }
                ranked(&self.known_skills, &query, |(name, _)| name)
                    .into_iter()
                    .cloned()
                    .collect()
            }
        };
        items.truncate(if kind == Kind::Command {
            COMMANDS.len()
        } else {
            MAX_SUGGESTIONS
        });
        if items.is_empty() {
            let why = match (&self.skills, kind) {
                (_, Kind::Command) => NO_MATCH,
                (None, _) => SKILLS_OFF,
                (Some(_), _) if self.known_skills.is_empty() => NO_SKILLS,
                (Some(_), _) => NO_MATCH,
            };
            items.push((String::new(), why.get(self.lang).to_owned()));
        }
        let selected = if reopened {
            0
        } else {
            self.completion
                .as_ref()
                .map_or(0, |c| c.selected.min(items.len() - 1))
        };
        self.completion = Some(Completion {
            kind,
            start,
            items,
            selected,
        });
    }

    /// Puts the highlighted choice in place of what was typed. A command
    /// that needs nothing more runs on Enter (`run`).
    fn accept_completion(&mut self, run: bool) -> Action {
        let Some(c) = self.completion.take() else {
            return Action::None;
        };
        let Some((name, _)) = c.items.get(c.selected).filter(|(n, _)| !n.is_empty()) else {
            self.dismissed = Some(c.start);
            return Action::None;
        };
        if c.kind == Kind::Command {
            let takes_arg = COMMANDS.iter().any(|cmd| cmd.name == name && cmd.arg);
            if run && !takes_arg {
                self.input.take();
                return Action::Run(format!("/{name}"));
            }
            self.input.set(&format!("/{name} "));
            self.dismissed = Some(0);
            return Action::None;
        }
        let insert: Vec<char> = format!("@{name} ").chars().collect();
        let end = self.input.cursor;
        self.input.text.splice(c.start..end, insert.iter().copied());
        self.input.cursor = c.start + insert.len();
        Action::None
    }

    /// Ready skills named with @ in `text`.
    fn mentioned_skills(&self, text: &str) -> Vec<String> {
        let Some(skills) = &self.skills else {
            return Vec::new();
        };
        let ready = skills.ready();
        let mut names: Vec<String> = Vec::new();
        for word in text.split_whitespace() {
            let Some(name) = word.strip_prefix('@') else {
                continue;
            };
            let name =
                name.trim_end_matches(|c: char| c.is_ascii_punctuation() && c != '-' && c != '_');
            if ready.iter().any(|s| s.name == name) && !names.iter().any(|n| n == name) {
                names.push(name.to_owned());
            }
        }
        names
    }

    /// Everything Ctrl+K offers for `query`: commands, then sessions.
    fn palette_items(&self, query: &str) -> Vec<PaletteItem> {
        let commands: Vec<PaletteItem> = COMMANDS
            .iter()
            .map(|c| PaletteItem {
                label: format!("/{}", c.name),
                detail: c.help.get(self.lang).to_owned(),
                line: if c.arg {
                    format!("/{} ", c.name)
                } else {
                    format!("/{}", c.name)
                },
                edit: c.arg,
            })
            .collect();
        let sessions: Vec<PaletteItem> = self
            .sessions
            .iter()
            .filter(|s| s.name != self.session)
            .map(|s| PaletteItem {
                label: s.name.clone(),
                detail: PALETTE_SESSION.get(self.lang).to_owned(),
                line: format!("/session {}", s.name),
                edit: false,
            })
            .collect();
        let query = query.trim().trim_start_matches('/');
        ranked(&commands, query, |i| &i.label[1..])
            .into_iter()
            .chain(ranked(&sessions, query, |i| &i.label))
            .cloned()
            .collect()
    }

    fn palette_key(&mut self, key: KeyEvent) -> Action {
        let Some(palette) = &mut self.palette else {
            return Action::None;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.palette = None,
            KeyCode::Char('c' | 'k') if ctrl => self.palette = None,
            KeyCode::Up => palette.selected = palette.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => palette.selected += 1,
            KeyCode::Enter => {
                let query = palette.query.string();
                let items = self.palette_items(&query);
                let selected = self.palette.as_ref().map_or(0, |p| p.selected);
                self.palette = None;
                let Some(item) = items.get(selected.min(items.len().saturating_sub(1))) else {
                    return Action::None;
                };
                if item.edit {
                    self.input.set(&item.line);
                    self.focus = Focus::Input;
                    return Action::None;
                }
                return Action::Run(item.line.clone());
            }
            _ => {
                if palette.query.edit_key(&key) {
                    palette.selected = 0;
                }
            }
        }
        if let Some(query) = self.palette.as_ref().map(|p| p.query.string()) {
            let count = self.palette_items(&query).len();
            if let Some(palette) = &mut self.palette {
                palette.selected = palette.selected.min(count.saturating_sub(1));
            }
        }
        Action::None
    }

    fn update_context<M: Model, T: Tools>(&mut self, agent: &Agent<M, T>) {
        self.context = agent.context_use(&self.session).ok();
    }

    /// Reads the session list again, with the open session in it even
    /// before it has messages.
    fn refresh_sessions<M: Model, T: Tools>(&mut self, agent: &Agent<M, T>) {
        self.sessions = agent.store.sessions().unwrap_or_default();
        if !self.sessions.iter().any(|s| s.name == self.session) {
            self.sessions.insert(
                0,
                SessionSummary {
                    name: self.session.clone(),
                    messages: 0,
                    updated_at: crate::store::now(),
                },
            );
        }
        self.sidebar_selected = self
            .sidebar_selected
            .min(self.sessions.len().saturating_sub(1));
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
                output: None,
                started: Instant::now(),
                elapsed: None,
                expanded: false,
            }),
            AgentEvent::ToolEnd { name, output: text } => {
                let open =
                    self.entries.iter_mut().rev().find(
                        |e| matches!(e, Entry::Tool { name: n, output: None, .. } if *n == name),
                    );
                if let Some(Entry::Tool {
                    output,
                    started,
                    elapsed,
                    ..
                }) = open
                {
                    let mut text = text;
                    if text.len() > MAX_TOOL_OUTPUT {
                        let mut cut = MAX_TOOL_OUTPUT;
                        while !text.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        text.truncate(cut);
                    }
                    *output = Some(text);
                    *elapsed = Some(started.elapsed());
                }
            }
            AgentEvent::FollowUp(_) => {}
        }
    }

    /// The turn ended; `reply` is the whole answer already streamed.
    fn finished(&mut self, result: Result<String>) {
        self.running = false;
        self.stopping = false;
        // A stopped turn leaves tools that never reported back.
        for entry in &mut self.entries {
            if let Entry::Tool {
                output: output @ None,
                started,
                elapsed,
                ..
            } = entry
            {
                *output = Some(String::new());
                *elapsed = Some(started.elapsed());
            }
        }
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

    /// Opens or closes every tool output: all open unless all already are.
    fn toggle_tools(&mut self) {
        let open = self.entries.iter().any(|e| {
            matches!(
                e,
                Entry::Tool {
                    expanded: false,
                    ..
                }
            )
        });
        for entry in &mut self.entries {
            if let Entry::Tool { expanded, .. } = entry {
                *expanded = open;
            }
        }
    }

    /// Whether the session list is on screen at `width` columns.
    fn sidebar_shown(&self, width: u16) -> bool {
        self.sidebar && width >= SIDEBAR_MIN_WIDTH
    }

    fn key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        if self.approval.is_some() {
            return self.edit(key);
        }
        if self.palette.is_some() {
            return self.palette_key(key);
        }
        if self.focus == Focus::Sidebar {
            return self.sidebar_key(key);
        }
        if let Some(c) = &mut self.completion {
            let plain = !key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT);
            match key.code {
                KeyCode::Up => {
                    c.selected = c.selected.checked_sub(1).unwrap_or(c.items.len() - 1);
                    return Action::None;
                }
                KeyCode::Down => {
                    c.selected = (c.selected + 1) % c.items.len();
                    return Action::None;
                }
                KeyCode::Tab => {
                    let action = self.accept_completion(false);
                    self.refresh_completion();
                    return action;
                }
                KeyCode::Enter if plain => {
                    let action = self.accept_completion(true);
                    self.refresh_completion();
                    return action;
                }
                KeyCode::Esc => {
                    self.dismissed = Some(c.start);
                    self.completion = None;
                    return Action::None;
                }
                _ => {}
            }
        }
        let action = self.edit(key);
        self.refresh_completion();
        action
    }

    /// Keys while the session list has focus.
    fn sidebar_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let last = self.sessions.len().saturating_sub(1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') if !ctrl => {
                self.sidebar_selected = self.sidebar_selected.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') if !ctrl => {
                self.sidebar_selected = (self.sidebar_selected + 1).min(last)
            }
            KeyCode::Home => self.sidebar_selected = 0,
            KeyCode::End => self.sidebar_selected = last,
            KeyCode::Enter => {
                self.focus = Focus::Input;
                if let Some(s) = self.sessions.get(self.sidebar_selected)
                    && s.name != self.session
                {
                    return Action::Run(format!("/session {}", s.name));
                }
            }
            KeyCode::Esc | KeyCode::Tab | KeyCode::BackTab => self.focus = Focus::Input,
            // Global keys work from here too.
            _ if ctrl => {
                self.focus = Focus::Input;
                return self.key(key);
            }
            // Typing goes back to the input.
            KeyCode::Char(_) => {
                self.focus = Focus::Input;
                return self.key(key);
            }
            _ => {}
        }
        Action::None
    }

    /// Keys while no popup list is open.
    fn edit(&mut self, key: KeyEvent) -> Action {
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
            KeyCode::Char('k') if ctrl => self.palette = Some(Palette::default()),
            KeyCode::Char('n') if ctrl => return Action::Run("/new".into()),
            KeyCode::Char('b') if ctrl => self.sidebar = !self.sidebar,
            KeyCode::Char('o') if ctrl => self.toggle_tools(),
            KeyCode::Char('l') if ctrl => {
                self.scroll = 0;
                self.repaint = true;
            }
            // Only when the list is on screen (it hides on narrow terminals).
            KeyCode::Tab if self.sidebar && self.hits.borrow().sidebar.width > 0 => {
                self.focus = Focus::Sidebar;
                self.sidebar_selected = self
                    .sessions
                    .iter()
                    .position(|s| s.name == self.session)
                    .unwrap_or(0);
            }
            KeyCode::PageUp => self.scroll += 10,
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Up if shift => self.scroll += 1,
            KeyCode::Down if shift => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Up => self.recall_older(),
            KeyCode::Down => self.recall_newer(),
            _ => {
                self.input.edit_key(&key);
            }
        }
        Action::None
    }

    /// The wheel scrolls, a click on a session opens it, a click on a tool
    /// line opens or closes its output.
    fn mouse(&mut self, event: MouseEvent) -> Action {
        let at = Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::ScrollUp => self.scroll += WHEEL_LINES,
            MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_sub(WHEEL_LINES),
            MouseEventKind::Down(MouseButton::Left)
                if self.approval.is_none() && self.palette.is_none() =>
            {
                let hits = self.hits.borrow();
                if hits.sidebar.contains(at) {
                    let row = event.row.checked_sub(hits.sidebar_first_row);
                    let name = row.and_then(|r| hits.sidebar_rows.get(usize::from(r)).cloned());
                    drop(hits);
                    self.focus = Focus::Input;
                    if let Some(name) = name
                        && name != self.session
                    {
                        return Action::Run(format!("/session {name}"));
                    }
                } else if hits.body.contains(at) {
                    let row = usize::from(event.row - hits.body.y);
                    let entry = hits.body_rows.get(row).copied().flatten();
                    drop(hits);
                    self.focus = Focus::Input;
                    if let Some(Some(Entry::Tool { expanded, .. })) =
                        entry.map(|i| self.entries.get_mut(i))
                    {
                        *expanded = !*expanded;
                    }
                } else {
                    drop(hits);
                    self.focus = Focus::Input;
                }
            }
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
        let trimmed = text.trim();
        if !trimmed.is_empty() && self.sent.last().map(String::as_str) != Some(trimmed) {
            self.sent.push(trimmed.to_owned());
        }
        self.run(agent, text)
    }

    /// Runs a command line, or turns text into a message for the agent.
    fn run<M: Model, T: Tools>(&mut self, agent: &Agent<M, T>, text: String) -> Option<Outgoing> {
        self.scroll = 0;
        let trimmed = text.trim();
        if is_shell_command(trimmed) {
            self.open_shell = true;
            return None;
        }
        if trimmed.starts_with('/') && !trimmed.starts_with("/identity") && trimmed != "/compact" {
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
        let skills = self.mentioned_skills(&text);
        let text = if skills.is_empty() {
            text
        } else {
            format!(
                "{}\n\n{}",
                text.trim_end(),
                SKILL_HINT.replace("{}", &skills.join(", "))
            )
        };
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
            "/clear" => {
                self.entries.clear();
                self.cache.borrow_mut().clear();
                self.repaint = true;
            }
            "/sidebar" => self.sidebar = !self.sidebar,
            "/config" if self.running => self.note(CONFIG_BUSY.get(self.lang)),
            "/config" => self.open_config = true,
            "/expand" => self.toggle_tools(),
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
            "/skills" => match &self.skills {
                None => self.note(SKILLS_OFF.get(self.lang)),
                Some(skills) => {
                    let all = skills.scan();
                    if all.is_empty() {
                        self.note(NO_SKILLS.get(self.lang));
                    } else {
                        let rows: Vec<String> = all
                            .iter()
                            .map(|s| {
                                format!(
                                    "@{}  {} [{}]",
                                    s.name,
                                    one_line(&s.description, 80),
                                    s.status()
                                )
                            })
                            .collect();
                        self.note(rows.join("\n"));
                    }
                }
            },
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
        self.cache.borrow_mut().clear();
        self.update_context(agent);
        self.refresh_sessions(agent);
        self.note(SWITCHED.fill(self.lang, &[&self.session]));
    }
}

/// `items` whose key matches `query`: names that start with it, then ones
/// that contain it, then ones that have its letters in order.
fn ranked<'a, T>(items: &'a [T], query: &str, key: impl Fn(&T) -> &str) -> Vec<&'a T> {
    let query = query.to_lowercase();
    let mut scored: Vec<(u8, usize, &T)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            let name = key(item).to_lowercase();
            let score = if name.starts_with(&query) {
                0
            } else if name.contains(&query) {
                1
            } else {
                let mut rest = name.chars();
                if query.chars().all(|q| rest.any(|c| c == q)) {
                    2
                } else {
                    return None;
                }
            };
            Some((score, i, item))
        })
        .collect();
    scored.sort_by_key(|&(score, i, _)| (score, i));
    scored.into_iter().map(|(_, _, item)| item).collect()
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

impl StderrToFile {
    /// A copy of the stderr the window started with, for a child that
    /// takes over the terminal.
    fn original(&self) -> Option<std::os::fd::OwnedFd> {
        use std::os::fd::BorrowedFd;
        let saved = self.0?;
        // SAFETY: `saved` stays open until drop, which needs `&mut self`.
        let fd = unsafe { BorrowedFd::borrow_raw(saved) };
        fd.try_clone_to_owned().ok()
    }
}

/// Runs the person's shell on the terminal and waits for it to exit. The
/// window must already be off the screen. Like system(3), Ctrl+C and
/// Ctrl+\ go to the shell only: this process ignores them while it waits,
/// and the shell gets them back.
fn run_shell(
    shell: &str,
    stderr: Option<std::os::fd::OwnedFd>,
) -> std::io::Result<std::process::ExitStatus> {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new(shell);
    command.env("RUNO_SHELL", "1");
    if let Some(fd) = stderr {
        command.stderr(fd);
    }
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGQUIT, libc::SIG_DFL);
            Ok(())
        });
    }
    // SAFETY: the old handlers are put back below, whatever happens.
    let (int, quit) = unsafe {
        (
            libc::signal(libc::SIGINT, libc::SIG_IGN),
            libc::signal(libc::SIGQUIT, libc::SIG_IGN),
        )
    };
    let status = command.status();
    // SAFETY: restores what `signal` returned above.
    unsafe {
        libc::signal(libc::SIGINT, int);
        libc::signal(libc::SIGQUIT, quit);
    }
    status
}

/// `$SHELL` when it names a program, else `/bin/sh`.
fn shell_path() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/bin/sh".to_owned())
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
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
        ratatui::restore();
    }
}

enum Step {
    Act(Action),
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
    config: &Path,
    skills: Option<Skills>,
) -> Result<()> {
    let lang = crate::i18n::current();
    let identity = agent.store.identity()?;
    let name = identity
        .as_ref()
        .map_or_else(|| "Runo".to_owned(), |i| i.name.clone());
    let mut app = App::new(lang, name, model, session);
    if let Some(emoji) = identity.as_ref().and_then(|i| i.emoji.clone()) {
        app.icon = emoji;
    }
    app.skills = skills;
    app.entries = load_history(agent, &app.session);
    app.update_context(agent);
    app.refresh_sessions(agent);
    if identity.is_none() {
        app.note(FIRST_START_NOTE.get(lang));
    }

    let stderr = StderrToFile::new(&state.join("tui.log"));
    // Raw mode, the alternate screen and the panic hook, then a backend
    // that stays clean on terminals that draw wide characters differently.
    drop(ratatui::init());
    let mut terminal = ratatui::Terminal::new(backend::RowBackend::new(std::io::stdout()))?;
    let _restore = Restore;
    crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)?;
    let mut keys = EventStream::new();
    let (events, mut inbox) = mpsc::unbounded_channel();
    let mut turn: Option<Turn> = None;
    let mut cancel = Cancel::default();
    let mut tick = tokio::time::interval(Duration::from_millis(100));

    loop {
        if std::mem::take(&mut app.open_config) {
            // The editor reads the terminal itself: stop reading keys here,
            // and give it a cleared screen.
            drop(keys);
            crossterm::terminal::disable_raw_mode()?;
            crossterm::execute!(
                std::io::stdout(),
                crossterm::event::DisableMouseCapture,
                crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
                crossterm::cursor::MoveTo(0, 0),
                crossterm::cursor::Show
            )?;
            let edited = tokio::task::block_in_place(|| crate::setup::run(config, lang));
            crossterm::terminal::enable_raw_mode()?;
            crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)?;
            keys = EventStream::new();
            app.repaint = true;
            match edited {
                Ok(Some(true)) => app.note(CONFIG_APPLIED.get(lang)),
                Ok(Some(false)) => app.note(CONFIG_SAVED.get(lang)),
                Ok(None) => {}
                Err(err) => app.entries.push(Entry::Error(format!("{err:#}"))),
            }
        }
        if std::mem::take(&mut app.open_shell) {
            // The shell gets the terminal as the person left it: the main
            // screen, cooked mode and the real stderr. A running reply waits.
            drop(keys);
            crossterm::terminal::disable_raw_mode()?;
            crossterm::execute!(
                std::io::stdout(),
                crossterm::event::DisableMouseCapture,
                crossterm::terminal::LeaveAlternateScreen,
                crossterm::cursor::Show
            )?;
            let shell = shell_path();
            println!("{}", SHELL_BANNER.fill(lang, &[&shell]));
            let status = tokio::task::block_in_place(|| run_shell(&shell, stderr.original()));
            crossterm::execute!(
                std::io::stdout(),
                crossterm::terminal::EnterAlternateScreen,
                crossterm::event::EnableMouseCapture
            )?;
            crossterm::terminal::enable_raw_mode()?;
            keys = EventStream::new();
            app.repaint = true;
            match status {
                Ok(status) => app.note(SHELL_BACK.fill(lang, &[&status.to_string()])),
                Err(err) => app.entries.push(Entry::Error(
                    SHELL_FAILED.fill(lang, &[&shell, &err.to_string()]),
                )),
            }
        }
        if std::mem::take(&mut app.repaint) {
            terminal.clear()?;
        }
        terminal.draw(|frame| app.draw(frame))?;
        let step = tokio::select! {
            // Agent events before the turn's end, so no delta comes after it.
            biased;
            Some(event) = inbox.recv() => Step::Ui(event),
            event = keys.next() => match event {
                Some(Ok(Event::Key(key))) => Step::Act(app.key(key)),
                Some(Ok(Event::Mouse(mouse))) => Step::Act(app.mouse(mouse)),
                Some(Ok(Event::Paste(text))) => {
                    let target = match &mut app.palette {
                        Some(palette) => &mut palette.query,
                        None => &mut app.input,
                    };
                    for c in text.chars() {
                        target.insert(if c == '\r' { '\n' } else { c });
                    }
                    app.focus = Focus::Input;
                    app.refresh_completion();
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
            Step::Act(action) => {
                let message = match action {
                    Action::None => None,
                    Action::Send => app.submit(agent),
                    Action::Run(line) => app.run(agent, line),
                    Action::Stop => {
                        // Stopping also drops what was waiting to go after it.
                        app.queue.clear();
                        app.stopping = true;
                        cancel.cancel();
                        None
                    }
                    Action::Quit => break,
                };
                if let Some(message) = message {
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
                app.update_context(agent);
                app.refresh_sessions(agent);
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
mod tests;
