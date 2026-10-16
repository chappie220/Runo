//! `runo console`: the Agent's window on a screen with no other graphics.
//! The kernel's console cannot draw Chinese, so this is a small terminal of
//! its own: it draws on the framebuffer with WenQuanYi Zen Hei, reads
//! keyboards from evdev, has a Chinese input method's bar on the bottom row,
//! and runs `runo tui` in a pseudo-terminal (design 4.7).

mod font;
pub mod ime;
mod keys;
mod render;
mod term;

#[cfg(target_os = "linux")]
mod fb;
#[cfg(target_os = "linux")]
mod input;

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::i18n::{Lang, Tr, tr};
use ime::InputMethod;

const NO_SCREEN: Tr = tr(
    "console: cannot draw on the screen ({}); the Agent runs on the plain console",
    "console：无法在屏幕上绘制（{}），Agent 改在普通控制台上运行",
);
const NO_FONT: Tr = tr(
    "console: cannot load the font ({}); the Agent runs on the plain console",
    "console：无法加载字体（{}），Agent 改在普通控制台上运行",
);
const NO_GRAPHICS: Tr = tr(
    "console: the kernel's console may draw over the screen: {}",
    "console：内核控制台可能会覆盖画面：{}",
);
const ENDED: Tr = tr(
    "runo tui ended ({}); it starts again in a moment",
    "runo tui 已退出（{}），稍后重新启动",
);

#[cfg(test)]
pub const ALL: &[Tr] = &[NO_SCREEN, NO_FONT, NO_GRAPHICS, ENDED];

pub struct Options {
    pub config: Option<PathBuf>,
    pub lang: Option<Lang>,
    /// For `runo tui`.
    pub args: Vec<OsString>,
}

/// `runo tui` with the global options this was given.
fn tui_args(options: &Options) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    if let Some(config) = &options.config {
        args.push("--config".into());
        args.push(config.into());
    }
    if let Some(lang) = options.lang {
        use clap::ValueEnum;
        if let Some(value) = lang.to_possible_value() {
            args.push("--lang".into());
            args.push(value.get_name().into());
        }
    }
    args.push("tui".into());
    args.extend(options.args.iter().cloned());
    args
}

/// Becomes `runo tui` on the kernel's console, so the device stays usable
/// without a screen to draw on.
fn plain(options: &Options) -> Result<i32> {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(std::env::current_exe()?)
        .args(tui_args(options))
        .exec();
    Err(err).context("starting runo tui")
}

/// The font file and size: `RUNO_CONSOLE_FONT`, `RUNO_CONSOLE_FONT_SIZE`
/// (pixels per em), else the installed font at a size for `width`.
fn font_settings(width: usize) -> (PathBuf, f32) {
    let path = std::env::var_os("RUNO_CONSOLE_FONT")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| font::DEFAULT_PATH.into());
    let size = std::env::var("RUNO_CONSOLE_FONT_SIZE")
        .ok()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .filter(|s| (6.0..=256.0).contains(s))
        .unwrap_or_else(|| font::default_size(width));
    (path, size)
}

#[cfg(not(target_os = "linux"))]
pub async fn run(options: Options, _ime: Option<Box<dyn InputMethod>>) -> Result<i32> {
    plain(&options)
}

/// Runs the console until `runo tui` ends; returns its exit code.
#[cfg(target_os = "linux")]
pub async fn run(options: Options, ime: Option<Box<dyn InputMethod>>) -> Result<i32> {
    let mut screen = match fb::Framebuffer::open(std::path::Path::new("/dev/fb0")) {
        Ok(screen) => screen,
        Err(err) => {
            eprintln!("{}", NO_SCREEN.with(&[&format!("{err:#}")]));
            return plain(&options);
        }
    };
    let (path, size) = font_settings(screen.width());
    let font = match font::Font::load(&path, size) {
        Ok(font) => font,
        Err(err) => {
            eprintln!("{}", NO_FONT.with(&[&format!("{err:#}")]));
            return plain(&options);
        }
    };
    if let Err(err) = fb::graphics_mode() {
        eprintln!("{}", NO_GRAPHICS.with(&[&format!("{err:#}")]));
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        fb::text_mode();
        previous(info);
    }));
    let result = session::run(&options, &mut screen, font, ime).await;
    fb::text_mode();
    result
}

#[cfg(target_os = "linux")]
mod session {
    use std::os::fd::{AsRawFd, OwnedFd, RawFd};
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    use std::time::Duration;

    use anyhow::{Context, Result};
    use tokio::io::unix::AsyncFd;
    use tokio::signal::unix::{SignalKind, signal};
    use tokio::sync::mpsc;
    use tokio::time::Instant;

    use super::fb::Framebuffer;
    use super::font::Font;
    use super::ime::{InputMethod, Typing};
    use super::input::{Keyboards, Message};
    use super::keys::{EV_KEY, Event, Keyboard};
    use super::render::{Canvas, Renderer};
    use super::term::Term;
    use super::{ENDED, Options, tui_args};
    use crate::daemon::pty;

    /// The fastest the screen is redrawn.
    const FRAME: Duration = Duration::from_millis(16);
    /// How often keyboards are looked for and the screen's mode checked.
    const SCAN: Duration = Duration::from_secs(2);
    /// How long an unexpected end stays on the screen.
    const ENDED_PAUSE: Duration = Duration::from_secs(3);

    enum End {
        Exited(ExitStatus),
        Signal(i32),
    }

    fn read_fd(fd: RawFd, buffer: &mut [u8]) -> std::io::Result<usize> {
        // SAFETY: reads into a valid buffer from an open descriptor.
        let n = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    fn write_fd(fd: RawFd, data: &[u8]) -> std::io::Result<usize> {
        // SAFETY: writes from a valid slice to an open descriptor.
        let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    /// The screen: what is drawn and where it goes.
    struct Display<'a> {
        screen: &'a mut Framebuffer,
        canvas: Canvas,
        renderer: Renderer,
    }

    impl Display<'_> {
        fn draw(&mut self, term: &Term, typing: &Typing, note: Option<&str>) {
            let cols = self.renderer.term_size().0;
            let bar = typing.bar(cols, crate::i18n::current(), note);
            let damage = self.renderer.draw(term, &bar, &mut self.canvas);
            self.screen.present(&self.canvas, &damage);
        }
    }

    pub async fn run(
        options: &Options,
        screen: &mut Framebuffer,
        font: Font,
        ime: Option<Box<dyn InputMethod>>,
    ) -> Result<i32> {
        let renderer = Renderer::new(font, screen.width(), screen.height());
        let canvas = Canvas::new(screen.width(), screen.height());
        let mut display = Display {
            screen,
            canvas,
            renderer,
        };
        let (cols, rows) = display.renderer.term_size();
        let mut term = Term::new(cols, rows);
        let (master, slave) = pty::open_pty(cols as u16, rows as u16)?;
        let mut command = tokio::process::Command::new(std::env::current_exe()?);
        command
            .args(tui_args(options))
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor");
        let mut child = pty::spawn_on(command, slave).context("starting runo tui")?;
        let master: AsyncFd<OwnedFd> = AsyncFd::new(master)?;

        let (sender, mut messages) = mpsc::unbounded_channel();
        let mut keyboards = Keyboards::new(sender);
        keyboards.scan();
        let mut keyboard = Keyboard::default();
        let mut typing = Typing::new(ime);
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sighup = signal(SignalKind::hangup())?;
        let mut scan = tokio::time::interval_at(Instant::now() + SCAN, SCAN);
        scan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut buffer = vec![0u8; 64 * 1024];
        // Keys and answers for the program.
        let mut outgoing: Vec<u8> = Vec::new();
        let mut open = true;
        let mut draw_at = Some(Instant::now());
        let mut last_draw = Instant::now();
        let mut scans = 0u32;

        let end = loop {
            tokio::select! {
                status = child.wait() => break End::Exited(status?),
                ready = master.readable(), if open => {
                    let mut guard = ready?;
                    match guard.try_io(|fd| read_fd(fd.as_raw_fd(), &mut buffer)) {
                        // EIO: the program and all it started are gone.
                        Ok(Ok(0)) | Ok(Err(_)) => open = false,
                        Ok(Ok(n)) => {
                            term.advance(&buffer[..n]);
                            outgoing.extend(term.take_replies());
                            draw_at.get_or_insert(last_draw + FRAME);
                        }
                        Err(_would_block) => {}
                    }
                }
                ready = master.writable(), if open && !outgoing.is_empty() => {
                    let mut guard = ready?;
                    match guard.try_io(|fd| write_fd(fd.as_raw_fd(), &outgoing)) {
                        Ok(Ok(n)) => {
                            outgoing.drain(..n);
                        }
                        Ok(Err(_)) => outgoing.clear(),
                        Err(_would_block) => {}
                    }
                }
                Some(message) = messages.recv() => match message {
                    Message::Events(events) => {
                        for (kind, code, value) in events {
                            if kind != EV_KEY {
                                continue;
                            }
                            match keyboard.key(code, value) {
                                Some(Event::Press(press)) => {
                                    outgoing.extend(typing.press(&press, term.app_cursor()));
                                }
                                Some(Event::ShiftTap) => outgoing.extend(typing.toggle()),
                                Some(Event::Caps(on)) => keyboards.caps_light(on),
                                None => {}
                            }
                        }
                        // The bar may show something else.
                        draw_at.get_or_insert(last_draw + FRAME);
                    }
                    Message::Gone(path) => keyboards.gone(&path),
                },
                _ = tokio::time::sleep_until(draw_at.unwrap_or_else(Instant::now)), if draw_at.is_some() => {
                    display.draw(&term, &typing, None);
                    last_draw = Instant::now();
                    draw_at = None;
                }
                _ = scan.tick() => {
                    keyboards.scan();
                    match display.screen.reopen_if_changed() {
                        Ok(true) => {
                            let (width, height) = (display.screen.width(), display.screen.height());
                            display.canvas = Canvas::new(width, height);
                            display.renderer.resize(width, height);
                            let (cols, rows) = display.renderer.term_size();
                            term.resize(cols, rows);
                            pty::set_size(master.as_raw_fd(), cols as u16, rows as u16);
                            draw_at = Some(Instant::now());
                        }
                        Ok(false) => {}
                        Err(err) => eprintln!("console: {err:#}"),
                    }
                    scans += 1;
                    if scans.is_multiple_of(15) {
                        typing.save();
                    }
                }
                _ = sigterm.recv() => break End::Signal(libc::SIGTERM),
                _ = sigint.recv() => break End::Signal(libc::SIGINT),
                _ = sighup.recv() => break End::Signal(libc::SIGHUP),
            }
        };
        typing.save();
        match end {
            End::Signal(number) => {
                if let Some(pid) = child.id() {
                    // Its process group: the Agent and what it started.
                    // SAFETY: plain kill(2).
                    unsafe { libc::kill(-(pid as i32), libc::SIGHUP) };
                }
                Ok(128 + number)
            }
            End::Exited(status) => {
                if !status.success() {
                    // Show what it said before init starts it again.
                    while open {
                        match read_fd(master.as_raw_fd(), &mut buffer) {
                            Ok(n) if n > 0 => term.advance(&buffer[..n]),
                            _ => open = false,
                        }
                    }
                    let how = match status.code() {
                        Some(code) => code.to_string(),
                        None => format!("signal {}", status.signal().unwrap_or(0)),
                    };
                    display.draw(&term, &typing, Some(&ENDED.with(&[&how])));
                    tokio::time::sleep(ENDED_PAUSE).await;
                }
                Ok(status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_are_translated() {
        crate::i18n::assert_complete(ALL);
    }

    #[test]
    fn passes_global_options_to_the_tui() {
        let options = Options {
            config: Some("/data/runo/config.toml".into()),
            lang: Some(Lang::Zh),
            args: vec!["-s".into(), "phone".into()],
        };
        assert_eq!(
            tui_args(&options),
            [
                "--config",
                "/data/runo/config.toml",
                "--lang",
                "zh",
                "tui",
                "-s",
                "phone"
            ]
            .map(OsString::from)
        );
        let options = Options {
            config: None,
            lang: None,
            args: Vec::new(),
        };
        assert_eq!(tui_args(&options), [OsString::from("tui")]);
    }
}
