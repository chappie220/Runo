//! The screens: board, image, Wi-Fi, card, confirm, writing, done.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Gauge, List, ListItem, ListState, Paragraph, Wrap};

use crate::disk::{self, Disk};
use crate::text::{size, t};
use crate::write::{self, Device, Flasher, Settings, Wifi};
use crate::{image, release};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Board {
    Pi5,
    Pi4,
}

impl Board {
    const ALL: [Board; 2] = [Board::Pi5, Board::Pi4];

    pub fn id(self) -> &'static str {
        match self {
            Board::Pi5 => "rpi5",
            Board::Pi4 => "rpi4",
        }
    }

    pub fn from_id(id: &str) -> Option<Board> {
        Board::ALL.into_iter().find(|b| b.id() == id)
    }

    fn label(self) -> &'static str {
        match self {
            Board::Pi5 => "Raspberry Pi 5",
            Board::Pi4 => "Raspberry Pi 4 / 400",
        }
    }
}

pub struct Options {
    pub image: Option<PathBuf>,
    pub board: Option<Board>,
    /// Write to this file instead of a card.
    pub to_file: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq)]
enum Source {
    Download,
    File(PathBuf),
    /// Type a path.
    Other,
}

#[derive(Clone, Debug)]
enum Target {
    Disk(Disk),
    File(PathBuf),
}

#[derive(Clone, Debug, PartialEq)]
enum Screen {
    Board,
    Source,
    Path,
    Wifi,
    Password,
    Disk,
    Confirm,
    Work,
    Done,
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Stage {
    Find,
    Download,
    Write,
    Verify,
}

/// What the writing thread is doing, for the screen.
struct Status {
    stage: Mutex<(Stage, Instant)>,
    done: AtomicU64,
    total: AtomicU64,
    /// Whether the writing is counted on the card, not in the file read.
    exact: AtomicBool,
    result: Mutex<Option<Result<(), String>>>,
}

impl Status {
    fn new() -> Self {
        Status {
            stage: Mutex::new((Stage::Find, Instant::now())),
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            exact: AtomicBool::new(false),
            result: Mutex::new(None),
        }
    }

    fn stage(&self, stage: Stage) {
        self.done.store(0, Ordering::Relaxed);
        self.total.store(0, Ordering::Relaxed);
        self.exact.store(false, Ordering::Relaxed);
        *self.stage.lock().unwrap() = (stage, Instant::now());
    }

    fn progress(&self, done: u64, total: u64) {
        self.done.store(done, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }

    /// How much of the image is on the card; from the first call on, the
    /// position in the file read no longer counts.
    fn written(&self, done: u64, total: u64) {
        self.exact.store(true, Ordering::Relaxed);
        self.progress(done, total);
    }

    /// How far into the image file reading has got: a rough measure,
    /// used only until the image's own length is known.
    fn read(&self, done: u64, total: u64) {
        if !self.exact.load(Ordering::Relaxed) {
            self.progress(done, total);
        }
    }
}

struct App {
    screen: Screen,
    board: Board,
    board_list: ListState,
    sources: Vec<Source>,
    source_list: ListState,
    source: Option<Source>,
    path: String,
    /// Network name, password, country.
    wifi_fields: [String; 3],
    wifi_field: usize,
    wifi: Option<Wifi>,
    /// Made up when the program starts; the person may change it.
    password_field: String,
    password: Option<String>,
    disks: Vec<Disk>,
    disk_list: ListState,
    disk_error: Option<String>,
    target: Option<Target>,
    confirm: String,
    message: Option<String>,
    status: Option<Arc<Status>>,
    quit_armed: bool,
    quit: bool,
    to_file: Option<PathBuf>,
    image_given: bool,
}

/// Image files in the places people download to.
fn local_images() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = std::env::current_dir() {
        dirs.push(dir);
    }
    let mut homes: Vec<PathBuf> = ["HOME", "USERPROFILE"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .collect();
    // Run through sudo, HOME may be root's.
    if let Some(user) = std::env::var_os("SUDO_USER") {
        homes.push(Path::new("/home").join(user));
    }
    for home in homes {
        dirs.push(home.join("Downloads"));
        dirs.push(home.join("下载"));
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        dirs.push(dir);
    }
    let mut found = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if image::is_image_name(&name)
                && path.is_file()
                && !found.iter().any(|p: &PathBuf| same(p, &path))
            {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

fn same(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

impl App {
    fn new(options: Options) -> Self {
        let board = options
            .board
            .or_else(|| {
                let name = options
                    .image
                    .as_ref()?
                    .file_name()?
                    .to_string_lossy()
                    .into_owned();
                Board::from_id(image::board_in_name(&name)?)
            })
            .unwrap_or(Board::Pi5);
        let mut app = App {
            screen: Screen::Board,
            board,
            board_list: ListState::default()
                .with_selected(Some(Board::ALL.iter().position(|b| *b == board).unwrap())),
            sources: Vec::new(),
            source_list: ListState::default().with_selected(Some(0)),
            source: None,
            path: String::new(),
            wifi_fields: [String::new(), String::new(), "CN".into()],
            wifi_field: 0,
            wifi: None,
            password_field: write::new_password(),
            password: None,
            disks: Vec::new(),
            disk_list: ListState::default(),
            disk_error: None,
            target: options.to_file.clone().map(Target::File),
            confirm: String::new(),
            message: None,
            status: None,
            quit_armed: false,
            quit: false,
            to_file: options.to_file,
            image_given: options.image.is_some(),
        };
        if !crate::text::chinese() {
            app.wifi_fields[2].clear();
        }
        if let Some(image) = options.image {
            app.source = Some(Source::File(image));
            app.screen = Screen::Wifi;
        }
        app
    }

    fn go(&mut self, screen: Screen) {
        self.message = None;
        match screen {
            Screen::Source => {
                self.sources = vec![Source::Download];
                self.sources
                    .extend(local_images().into_iter().map(Source::File));
                self.sources.push(Source::Other);
                // A file found is likelier wanted than a download.
                self.source_list
                    .select(Some(if self.sources.len() > 2 { 1 } else { 0 }));
            }
            Screen::Disk => self.refresh_disks(),
            Screen::Confirm => self.confirm.clear(),
            _ => {}
        }
        self.screen = screen;
    }

    fn refresh_disks(&mut self) {
        match disk::list() {
            Ok(disks) => {
                self.disk_error = None;
                let keep = self
                    .disk_list
                    .selected()
                    .and_then(|i| self.disks.get(i))
                    .map(|d| d.path.clone());
                self.disks = disks;
                let index = keep.and_then(|p| self.disks.iter().position(|d| d.path == p));
                self.disk_list.select(if self.disks.is_empty() {
                    None
                } else {
                    Some(index.unwrap_or(0))
                });
            }
            Err(err) => {
                self.disks.clear();
                self.disk_list.select(None);
                self.disk_error = Some(format!("{err:#}"));
            }
        }
    }

    fn back(&mut self) {
        let to = match self.screen {
            Screen::Board => {
                self.quit = true;
                return;
            }
            Screen::Source => Screen::Board,
            Screen::Path => Screen::Source,
            Screen::Wifi if self.image_given => {
                self.quit = true;
                return;
            }
            Screen::Wifi => Screen::Source,
            Screen::Password => Screen::Wifi,
            Screen::Disk => Screen::Password,
            Screen::Confirm if self.to_file.is_some() => Screen::Password,
            Screen::Confirm => Screen::Disk,
            Screen::Failed(_) => Screen::Confirm,
            Screen::Work | Screen::Done => return,
        };
        self.go(to);
    }

    fn key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            if self.screen == Screen::Work && !self.quit_armed {
                self.quit_armed = true;
            } else {
                self.quit = true;
            }
            return;
        }
        if key.code == KeyCode::Esc {
            self.back();
            return;
        }
        match self.screen.clone() {
            Screen::Board => match key.code {
                KeyCode::Up | KeyCode::Down => {
                    move_in(&mut self.board_list, Board::ALL.len(), key.code)
                }
                KeyCode::Enter => {
                    self.board = Board::ALL[self.board_list.selected().unwrap_or(0)];
                    self.go(Screen::Source);
                }
                KeyCode::Char('q') => self.quit = true,
                _ => {}
            },
            Screen::Source => match key.code {
                KeyCode::Up | KeyCode::Down => {
                    move_in(&mut self.source_list, self.sources.len(), key.code)
                }
                KeyCode::Enter => {
                    match self.sources[self.source_list.selected().unwrap_or(0)].clone() {
                        Source::Other => self.go(Screen::Path),
                        source => {
                            self.source = Some(source);
                            self.go(Screen::Wifi);
                        }
                    }
                }
                KeyCode::Char('q') => self.quit = true,
                _ => {}
            },
            Screen::Path => match key.code {
                KeyCode::Enter => {
                    let path = PathBuf::from(self.path.trim().trim_matches('"').trim_matches('\''));
                    if path.is_file() {
                        self.source = Some(Source::File(path));
                        self.go(Screen::Wifi);
                    } else {
                        self.message = Some(t("There is no such file.", "没有这个文件。").into());
                    }
                }
                _ => edit(&mut self.path, key),
            },
            Screen::Wifi => match key.code {
                KeyCode::Tab | KeyCode::Down => self.wifi_field = (self.wifi_field + 1) % 3,
                KeyCode::BackTab | KeyCode::Up => self.wifi_field = (self.wifi_field + 2) % 3,
                KeyCode::Enter if self.wifi_field < 2 && !self.wifi_fields[0].is_empty() => {
                    self.wifi_field += 1
                }
                KeyCode::Enter => self.submit_wifi(),
                _ => {
                    edit(&mut self.wifi_fields[self.wifi_field], key);
                    if self.wifi_field == 2 {
                        self.wifi_fields[2] = self.wifi_fields[2].to_ascii_uppercase();
                    }
                }
            },
            Screen::Password => match key.code {
                KeyCode::Enter => self.submit_password(),
                _ => edit(&mut self.password_field, key),
            },
            Screen::Disk => match key.code {
                KeyCode::Up | KeyCode::Down => {
                    move_in(&mut self.disk_list, self.disks.len(), key.code)
                }
                KeyCode::Char('r') => self.refresh_disks(),
                KeyCode::Enter => {
                    if let Some(disk) = self.disk_list.selected().and_then(|i| self.disks.get(i)) {
                        self.target = Some(Target::Disk(disk.clone()));
                        self.go(Screen::Confirm);
                    }
                }
                KeyCode::Char('q') => self.quit = true,
                _ => {}
            },
            Screen::Confirm => match key.code {
                KeyCode::Enter if self.confirm.trim().eq_ignore_ascii_case("yes") => self.start(),
                KeyCode::Enter => {
                    self.message = Some(t("Type yes to go on.", "请输入 yes 再按 Enter。").into())
                }
                _ => edit(&mut self.confirm, key),
            },
            Screen::Work => {}
            Screen::Done => {
                if matches!(key.code, KeyCode::Enter | KeyCode::Char('q')) {
                    self.quit = true;
                }
            }
            Screen::Failed(_) => match key.code {
                KeyCode::Enter => self.back(),
                KeyCode::Char('q') => self.quit = true,
                _ => {}
            },
        }
    }

    fn submit_wifi(&mut self) {
        let [ssid, password, country] = self.wifi_fields.clone();
        if ssid.is_empty() {
            self.wifi = None;
        } else {
            let wifi = Wifi {
                ssid,
                password,
                country: country.trim().to_string(),
            };
            if let Some(problem) = wifi.problem() {
                self.message = Some(problem.to_string());
                return;
            }
            self.wifi = Some(wifi);
        }
        self.go(Screen::Password);
    }

    fn submit_password(&mut self) {
        let password = self.password_field.clone();
        if password.is_empty() {
            self.password = None;
        } else {
            if let Some(problem) = write::password_problem(&password) {
                self.message = Some(problem.to_string());
                return;
            }
            self.password = Some(password);
        }
        self.go(if self.to_file.is_some() {
            Screen::Confirm
        } else {
            Screen::Disk
        });
    }

    fn start(&mut self) {
        let status = Arc::new(Status::new());
        self.status = Some(status.clone());
        self.quit_armed = false;
        self.screen = Screen::Work;
        let source = self.source.clone().expect("chosen");
        let target = self.target.clone().expect("chosen");
        let settings = Settings {
            wifi: self.wifi.clone(),
            password: self.password.clone(),
        };
        let board = self.board;
        std::thread::spawn(move || {
            let result =
                work(source, board, settings, target, &status).map_err(|err| format!("{err:#}"));
            *status.result.lock().unwrap() = Some(result);
        });
    }

    /// Moves on when the writing thread has finished.
    fn tick(&mut self) {
        if self.screen != Screen::Work {
            return;
        }
        let Some(status) = &self.status else { return };
        let result = status.result.lock().unwrap().take();
        match result {
            Some(Ok(())) => self.screen = Screen::Done,
            Some(Err(err)) => self.screen = Screen::Failed(err),
            None => {}
        }
    }
}

fn move_in(list: &mut ListState, len: usize, key: KeyCode) {
    if len == 0 {
        return;
    }
    let i = list.selected().unwrap_or(0);
    list.select(Some(match key {
        KeyCode::Up => (i + len - 1) % len,
        _ => (i + 1) % len,
    }));
}

fn edit(text: &mut String, key: KeyEvent) {
    match key.code {
        KeyCode::Backspace => {
            text.pop();
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => text.push(c),
        _ => {}
    }
}

fn work(
    source: Source,
    board: Board,
    settings: Settings,
    target: Target,
    status: &Status,
) -> Result<()> {
    let path = match source {
        Source::File(path) => path,
        Source::Download | Source::Other => {
            status.stage(Stage::Find);
            let found = release::find(board.id(), None)?;
            status.stage(Stage::Download);
            release::download(&found, &|d, t| status.progress(d, t))?
        }
    };
    status.stage(Stage::Write);
    let (mut dev, capacity): (Box<dyn Device>, Option<u64>) = match &target {
        Target::Disk(disk) => (disk::open(disk)?, Some(disk.size)),
        Target::File(path) => (
            Box::new(
                File::options()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(path)?,
            ),
            None,
        ),
    };
    let written = |d, t| status.written(d, t);
    let mut flasher = Flasher::new(dev.as_mut(), settings, capacity, &written);
    image::decode(&path, &mut flasher, &|d, t| status.read(d, t))?;
    let (len, hash) = flasher.finish()?;
    status.stage(Stage::Verify);
    write::verify(dev.as_mut(), len, hash, &|d, t| status.progress(d, t))?;
    drop(dev);
    if let Target::Disk(disk) = &target {
        disk::after(disk);
    }
    Ok(())
}

/// Runs the screens until the person is done.
pub fn run(options: Options) -> Result<bool> {
    let mut app = App::new(options);
    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        while !app.quit {
            app.tick();
            terminal.draw(|f| draw(f, &mut app))?;
            if event::poll(Duration::from_millis(200))?
                && let Event::Key(key) = event::read()?
                && key.kind != KeyEventKind::Release
            {
                app.key(key);
            }
        }
        Ok(())
    })();
    ratatui::restore();
    result?;
    Ok(app.screen == Screen::Done)
}

const ACCENT: Color = Color::Cyan;

fn draw(f: &mut Frame, app: &mut App) {
    let [head, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(f.area());
    f.render_widget(
        Line::from(vec![
            Span::styled(" Runo ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
            Span::raw(" "),
            Span::raw(t("SD card writer", "SD 卡烧录工具")),
            Span::styled(
                concat!("  v", env!("CARGO_PKG_VERSION")),
                Style::new().dim(),
            ),
        ]),
        head,
    );
    let (title, keys) = match &app.screen {
        Screen::Board => (
            t("1 · Which Raspberry Pi?", "1 · 选择树莓派型号"),
            t(
                "↑↓ choose  Enter next  q quit",
                "↑↓ 选择  Enter 下一步  q 退出",
            ),
        ),
        Screen::Source => (
            t("2 · Which system image?", "2 · 选择系统镜像"),
            t(
                "↑↓ choose  Enter next  Esc back",
                "↑↓ 选择  Enter 下一步  Esc 返回",
            ),
        ),
        Screen::Path => (
            t("2 · Image file", "2 · 镜像文件"),
            t("Enter next  Esc back", "Enter 下一步  Esc 返回"),
        ),
        Screen::Wifi => (
            t("3 · Wi-Fi (optional)", "3 · Wi-Fi（可选）"),
            t(
                "Tab/↑↓ field  Enter next  Esc back",
                "Tab/↑↓ 切换  Enter 下一步  Esc 返回",
            ),
        ),
        Screen::Password => (
            t("4 · Device password", "4 · 设备密码"),
            t("Enter next  Esc back", "Enter 下一步  Esc 返回"),
        ),
        Screen::Disk => (
            t("5 · Which card?", "5 · 选择 SD 卡"),
            t(
                "↑↓ choose  r refresh  Enter next  Esc back",
                "↑↓ 选择  r 刷新  Enter 下一步  Esc 返回",
            ),
        ),
        Screen::Confirm => (
            t("6 · Check and start", "6 · 确认并开始"),
            t(
                "type yes, Enter start  Esc back",
                "输入 yes 后 Enter 开始  Esc 返回",
            ),
        ),
        Screen::Work => (
            t("Writing", "正在写入"),
            t(
                "Ctrl+C twice to stop (the card is then unusable)",
                "按两次 Ctrl+C 中止（SD 卡需重新写入）",
            ),
        ),
        Screen::Done => (t("Done", "完成"), t("Enter quit", "Enter 退出")),
        Screen::Failed(_) => (
            t("It did not work", "失败了"),
            t("Enter try again  q quit", "Enter 重试  q 退出"),
        ),
    };
    f.render_widget(
        Line::from(Span::styled(format!(" {keys}"), Style::new().dim())),
        foot,
    );
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(ACCENT).bold(),
        ));
    let inner = block.inner(body);
    f.render_widget(block, body);
    let inner = inner.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 0,
    });
    match app.screen.clone() {
        Screen::Board => {
            let items: Vec<ListItem> = Board::ALL
                .iter()
                .map(|b| ListItem::new(b.label()))
                .collect();
            list(f, inner, items, &mut app.board_list, None);
        }
        Screen::Source => {
            let items: Vec<ListItem> = app
                .sources
                .iter()
                .map(|s| match s {
                    Source::Download => ListItem::new(Line::from(vec![
                        Span::raw(t("Download the latest from GitHub", "从 GitHub 下载最新版")),
                        Span::styled(
                            format!("  runo-{}.img.xz", app.board.id()),
                            Style::new().dim(),
                        ),
                    ])),
                    Source::File(path) => {
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let len = std::fs::metadata(path)
                            .map(|m| size(m.len()))
                            .unwrap_or_default();
                        ListItem::new(Line::from(vec![
                            Span::raw(name),
                            Span::styled(
                                format!(
                                    "  {len}  {}",
                                    path.parent()
                                        .map(|p| p.display().to_string())
                                        .unwrap_or_default()
                                ),
                                Style::new().dim(),
                            ),
                        ]))
                    }
                    Source::Other => ListItem::new(t("Another file…", "选择其他文件…")),
                })
                .collect();
            let hint = t(
                "A .img.xz from a release, the .zip from GitHub Actions, or an .img.",
                "可以是发布页的 .img.xz、GitHub Actions 下载的 .zip，或 .img。",
            );
            list(f, inner, items, &mut app.source_list, Some(hint));
        }
        Screen::Path => {
            let [a, b, c] = Layout::vertical([
                Constraint::Length(2),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .areas(inner);
            f.render_widget(
                Paragraph::new(t(
                    "The path of the image file (.img.xz, .zip or .img); you can drag the file here.",
                    "镜像文件的路径（.img.xz、.zip 或 .img），也可以把文件拖进这个窗口。",
                ))
                .wrap(Wrap { trim: true }),
                a,
            );
            input(f, b, "", &app.path, true, false);
            message(f, c, &app.message);
        }
        Screen::Wifi => {
            let rows = Layout::vertical([
                Constraint::Length(2),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .split(inner);
            f.render_widget(
                Paragraph::new(t(
                    "The network the Pi joins when it first starts. Leave the name empty to use a network cable.",
                    "树莓派第一次开机时连的无线网。名称留空就不设，用网线连网。",
                ))
                .wrap(Wrap { trim: true }),
                rows[0],
            );
            let labels = [
                t("Name", "名称"),
                t("Password", "密码"),
                t("Country", "国家"),
            ];
            for (i, label) in labels.into_iter().enumerate() {
                input(
                    f,
                    rows[i + 1],
                    label,
                    &app.wifi_fields[i],
                    app.wifi_field == i,
                    i == 1,
                );
            }
            message(f, rows[5], &app.message);
        }
        Screen::Password => {
            let [a, b, c] = Layout::vertical([
                Constraint::Length(4),
                Constraint::Length(2),
                Constraint::Min(1),
            ])
            .areas(inner);
            f.render_widget(
                Paragraph::new(t(
                    "The client app asks for it when it pairs with the Pi. With it set, the Pi opens pairing by itself on its first start, so it needs no screen. One was made up for you: write it down, or type your own (at least 8 characters). Empty: the Pi asks for one on its screen.",
                    "客户端配对时要输入这个密码。设置后，树莓派第一次开机会自动打开配对，不需要接屏幕。这里已经随机生成了一个：请记下来，或者改成你自己的（至少 8 个字符）。留空的话，树莓派会在屏幕上询问。",
                ))
                .wrap(Wrap { trim: true }),
                a,
            );
            input(
                f,
                b,
                t("Password", "密码"),
                &app.password_field,
                true,
                false,
            );
            message(f, c, &app.message);
        }
        Screen::Disk => {
            if let Some(err) = &app.disk_error {
                message(f, inner, &Some(err.clone()));
            } else if app.disks.is_empty() {
                f.render_widget(
                    Paragraph::new(t(
                        "No SD card found. Put the card in a card reader, then press r.",
                        "没找到 SD 卡。把 SD 卡插进读卡器，再按 r 刷新。",
                    ))
                    .wrap(Wrap { trim: true }),
                    inner,
                );
            } else {
                let items: Vec<ListItem> = app
                    .disks
                    .iter()
                    .map(|d| {
                        ListItem::new(Line::from(vec![
                            Span::styled(format!("{:>8}  ", size(d.size)), Style::new().bold()),
                            Span::raw(format!("{:<4} ", d.bus)),
                            Span::raw(
                                if d.name.is_empty() {
                                    t("(no name)", "（无名称）")
                                } else {
                                    &d.name
                                }
                                .to_string(),
                            ),
                            Span::styled(format!("  {}", d.path), Style::new().dim()),
                        ]))
                    })
                    .collect();
                let hint = t("Only removable disks are shown.", "这里只列出可移动磁盘。");
                list(f, inner, items, &mut app.disk_list, Some(hint));
            }
        }
        Screen::Confirm => confirm(f, inner, app),
        Screen::Work => work_screen(f, inner, app),
        Screen::Done => {
            let mut lines = vec![
                Line::styled(
                    t("The card is ready.", "SD 卡已写好。"),
                    Style::new().fg(Color::Green).bold(),
                ),
                Line::raw(""),
                Line::raw(t(
                    "Take it out, put it in the Pi and switch it on.",
                    "现在可以拔出 SD 卡，插进树莓派后接电开机。",
                )),
                Line::raw(t(
                    "On the first start the Pi grows the data partition to the whole card and restarts once by itself.",
                    "第一次开机会把数据分区扩展到整张卡，并自动重启一次。",
                )),
            ];
            if let Some(password) = &app.password {
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    Span::raw(t("Device password: ", "设备密码：")),
                    Span::styled(password.clone(), Style::new().fg(ACCENT).bold()),
                ]));
                lines.push(Line::raw(t(
                    "Once it is up, pair from the client app on the same network: check that the six digits match, then give this password.",
                    "开机后，在同一个网络里用客户端配对：确认两边的 6 位数字一致，再输入这个密码。",
                )));
            }
            if cfg!(windows) {
                lines.push(Line::raw(""));
                lines.push(Line::raw(t(
                    "If Windows offers to format the card, choose Cancel: it cannot read the Pi's partitions.",
                    "如果 Windows 提示要格式化磁盘，请点“取消”：它只是读不了树莓派的分区。",
                )));
            }
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
        }
        Screen::Failed(err) => {
            f.render_widget(
                Paragraph::new(vec![
                    Line::styled(
                        t("Writing failed:", "写入失败："),
                        Style::new().fg(Color::Red).bold(),
                    ),
                    Line::raw(""),
                    Line::raw(err),
                ])
                .wrap(Wrap { trim: true }),
                inner,
            );
        }
    }
}

fn list(
    f: &mut Frame,
    area: Rect,
    items: Vec<ListItem>,
    state: &mut ListState,
    hint: Option<&str>,
) {
    let [a, b] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(if hint.is_some() { 1 } else { 0 }),
    ])
    .areas(area);
    let list = List::new(items).highlight_symbol("› ").highlight_style(
        Style::new()
            .fg(ACCENT)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED),
    );
    f.render_stateful_widget(list, a, state);
    if let Some(hint) = hint {
        f.render_widget(Paragraph::new(hint).dim(), b);
    }
}

fn input(f: &mut Frame, area: Rect, label: &str, value: &str, active: bool, secret: bool) {
    let shown = if secret {
        "•".repeat(value.chars().count())
    } else {
        value.to_string()
    };
    let mut spans = Vec::new();
    if !label.is_empty() {
        spans.push(Span::styled(
            padded(label, 8),
            if active {
                Style::new().fg(ACCENT).bold()
            } else {
                Style::new()
            },
        ));
    }
    spans.push(Span::styled(shown, Style::new().underlined()));
    if active {
        spans.push(Span::styled("▏", Style::new().fg(ACCENT)));
    }
    f.render_widget(Line::from(spans), area);
}

/// `text` with spaces after it to `width` columns (Chinese takes two).
fn padded(text: &str, width: usize) -> String {
    let pad = width.saturating_sub(unicode_width::UnicodeWidthStr::width(text));
    format!("{text}{}", " ".repeat(pad))
}

fn message(f: &mut Frame, area: Rect, message: &Option<String>) {
    if let Some(m) = message {
        f.render_widget(
            Paragraph::new(m.as_str())
                .fg(Color::Red)
                .wrap(Wrap { trim: true }),
            area,
        );
    }
}

fn confirm(f: &mut Frame, area: Rect, app: &App) {
    let label = |s: &str| Span::styled(padded(s, 10), Style::new().dim());
    let source = match &app.source {
        Some(Source::File(p)) => p.display().to_string(),
        _ => format!(
            "{} (runo-{}.img.xz)",
            t("download from GitHub", "从 GitHub 下载"),
            app.board.id()
        ),
    };
    let target = match &app.target {
        Some(Target::Disk(d)) => format!("{}  {}  {}  ({})", size(d.size), d.bus, d.name, d.path),
        Some(Target::File(p)) => p.display().to_string(),
        None => String::new(),
    };
    let wifi = match &app.wifi {
        Some(w) => w.ssid.clone(),
        None => t("not set (network cable)", "不设置（用网线）").to_string(),
    };
    let mut lines = vec![
        Line::from(vec![
            label(t("Board", "型号")),
            Span::raw(app.board.label()),
        ]),
        Line::from(vec![label(t("Image", "镜像")), Span::raw(source)]),
        Line::from(vec![label("Wi-Fi"), Span::raw(wifi)]),
        Line::from(vec![
            label(t("Password", "设备密码")),
            match &app.password {
                Some(p) => Span::raw(p.clone()),
                None => Span::raw(t(
                    "not set (asked on the Pi's screen)",
                    "不设置（在树莓派屏幕上设置）",
                )),
            },
        ]),
        Line::from(vec![
            label(t("Card", "SD 卡")),
            Span::styled(target, Style::new().bold()),
        ]),
    ];
    if let Some(Source::File(p)) = &app.source
        && let Some(named) = p
            .file_name()
            .and_then(|n| image::board_in_name(&n.to_string_lossy()))
        && named != app.board.id()
    {
        lines.push(Line::styled(
            format!(
                "{} {named}",
                t(
                    "Note: the file's name says it is for",
                    "注意：文件名表明它是给这个型号的："
                )
            ),
            Style::new().fg(Color::Yellow),
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        t(
            "Everything on this card will be erased.",
            "这张卡上的所有数据都会被清除！",
        ),
        Style::new().fg(Color::Red).bold(),
    ));
    lines.push(Line::raw(t(
        "Type yes and press Enter to start:",
        "输入 yes 后按 Enter 开始：",
    )));
    let height = lines.len() as u16;
    let [a, b, c] = Layout::vertical([
        Constraint::Length(height),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(area);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), a);
    input(f, b, "", &app.confirm, true, false);
    message(f, c, &app.message);
}

fn work_screen(f: &mut Frame, area: Rect, app: &App) {
    let Some(status) = &app.status else { return };
    let (stage, since) = *status.stage.lock().unwrap();
    let done = status.done.load(Ordering::Relaxed);
    let total = status.total.load(Ordering::Relaxed);
    let what = match stage {
        Stage::Find => t("Looking for the latest release…", "正在查找最新版本…"),
        Stage::Download => t("Downloading the image", "正在下载镜像"),
        Stage::Write => t("Writing to the card", "正在写入 SD 卡"),
        Stage::Verify => t("Checking what was written", "正在校验写入的内容"),
    };
    let ratio = if total > 0 {
        (done as f64 / total as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let elapsed = since.elapsed().as_secs_f64();
    let mut detail = String::new();
    if total > 0 && elapsed > 2.0 && ratio > 0.01 {
        let left = (elapsed / ratio - elapsed).max(0.0) as u64;
        detail = format!("{} {}:{:02}", t("about", "大约还剩"), left / 60, left % 60);
    }
    let [a, b, c, d] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(1),
    ])
    .areas(area);
    f.render_widget(Line::styled(what, Style::new().bold()), a);
    f.render_widget(
        Gauge::default()
            .ratio(ratio)
            .gauge_style(Style::new().fg(ACCENT))
            .label(format!("{:.0}%", ratio * 100.0)),
        b,
    );
    f.render_widget(Paragraph::new(detail).dim(), c);
    let mut lines = vec![Line::raw(t(
        "Do not take the card out.",
        "写入过程中请不要拔出 SD 卡。",
    ))];
    if app.quit_armed {
        lines.push(Line::styled(
            t(
                "Press Ctrl+C again to stop; the card will then need writing again.",
                "再按一次 Ctrl+C 中止；之后需要重新写卡。",
            ),
            Style::new().fg(Color::Yellow),
        ));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), d);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn press(app: &mut App, code: KeyCode) {
        app.key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    fn screen(app: &mut App) -> String {
        let mut terminal = ratatui::Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    fn wait(app: &mut App) {
        let start = Instant::now();
        while app.screen == Screen::Work {
            assert!(start.elapsed() < Duration::from_secs(60), "still writing");
            std::thread::sleep(Duration::from_millis(20));
            app.tick();
        }
    }

    #[test]
    fn an_image_is_written_to_a_file_with_wifi() {
        let dir = tempfile::tempdir().unwrap();
        let img = crate::write::tests::image(9);
        let xz = dir.path().join("runo-rpi4.img.xz");
        let mut out = Vec::new();
        lzma_rs::xz_compress(&mut &img[..], &mut out).unwrap();
        std::fs::write(&xz, out).unwrap();
        let target = dir.path().join("card.img");
        let mut app = App::new(Options {
            image: Some(xz),
            board: None,
            to_file: Some(target.clone()),
        });
        assert_eq!(app.board, Board::Pi4);
        assert_eq!(app.screen, Screen::Wifi);
        typed(&mut app, "home");
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "short");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Wifi, "a short password is refused");
        assert!(screen(&mut app).contains("8 to 63"));
        app.wifi_fields[1] = "long enough".into();
        app.wifi_fields[2] = "CN".into();
        app.wifi_field = 2;
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Password);
        assert_eq!(app.password_field.len(), 12, "one is made up");
        app.password_field = "short".into();
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Password, "a short password is refused");
        assert!(screen(&mut app).contains("at least 8"));
        app.password_field = "pair with me".into();
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Confirm);
        let shown = screen(&mut app);
        assert!(shown.contains("Raspberry Pi 4"), "{shown}");
        assert!(shown.contains("home"), "{shown}");
        assert!(shown.contains("pair with me"), "{shown}");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Confirm, "nothing starts without yes");
        typed(&mut app, "yes");
        press(&mut app, KeyCode::Enter);
        wait(&mut app);
        assert_eq!(app.screen, Screen::Done, "{:?}", app.screen);
        let written = std::fs::read(&target).unwrap();
        assert_eq!(written.len(), img.len());
        assert_eq!(
            crate::write::tests::read_wifi(&written).unwrap(),
            "ssid=home\npassword=long enough\ncountry=CN\n"
        );
        assert_eq!(
            crate::write::tests::read_boot(&written, "runo-password.txt").unwrap(),
            "pair with me\n"
        );
        assert!(screen(&mut app).contains("pair with me"), "shown when done");
    }

    #[test]
    fn the_zip_from_github_actions_is_read() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let img = crate::write::tests::image(7);
        let mut xz = Vec::new();
        lzma_rs::xz_compress(&mut &img[..], &mut xz).unwrap();
        let zip_path = dir.path().join("runo-image-rpi5.zip");
        let mut zip = zip::ZipWriter::new(File::create(&zip_path).unwrap());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("SHA256SUMS-rpi5", options).unwrap();
        zip.write_all(b"x  runo-rpi5.img.xz\n").unwrap();
        zip.start_file("runo-rpi5.img.xz", options).unwrap();
        zip.write_all(&xz).unwrap();
        zip.finish().unwrap();
        let target = dir.path().join("card.img");
        let mut app = App::new(Options {
            image: Some(zip_path),
            board: None,
            to_file: Some(target.clone()),
        });
        assert_eq!(app.board, Board::Pi5);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Password);
        app.password_field.clear();
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Confirm);
        let shown = screen(&mut app);
        assert!(shown.contains("network cable"), "{shown}");
        assert!(shown.contains("not set"), "{shown}");
        typed(&mut app, "yes");
        press(&mut app, KeyCode::Enter);
        wait(&mut app);
        assert_eq!(app.screen, Screen::Done, "{:?}", app.screen);
        assert_eq!(std::fs::read(&target).unwrap(), img);
    }

    #[test]
    fn a_damaged_image_fails_and_can_be_tried_again() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("runo-rpi5.img.xz");
        std::fs::write(&bad, [0xfd, b'7', b'z', b'X', b'Z', 0, 1, 2, 3]).unwrap();
        let mut app = App::new(Options {
            image: Some(bad),
            board: None,
            to_file: Some(dir.path().join("card.img")),
        });
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "yes");
        press(&mut app, KeyCode::Enter);
        wait(&mut app);
        assert!(matches!(app.screen, Screen::Failed(_)), "{:?}", app.screen);
        assert!(screen(&mut app).contains("Writing failed"));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Confirm);
    }

    #[test]
    fn the_screens_go_forward_and_back() {
        let mut app = App::new(Options {
            image: None,
            board: None,
            to_file: Some("x".into()),
        });
        assert_eq!(app.screen, Screen::Board);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.board, Board::Pi4);
        assert_eq!(app.screen, Screen::Source);
        assert_eq!(app.sources[0], Source::Download);
        assert_eq!(*app.sources.last().unwrap(), Source::Other);
        app.source_list.select(Some(app.sources.len() - 1));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Path);
        typed(&mut app, "/no/such/file.img.xz");
        press(&mut app, KeyCode::Enter);
        assert!(screen(&mut app).contains("no such file"));
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.screen, Screen::Board);
        press(&mut app, KeyCode::Esc);
        assert!(app.quit);
    }
}
