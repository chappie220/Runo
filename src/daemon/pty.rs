//! Terminal sessions held by the daemon, not by a connection (4.3): the
//! Agent (`runo tui`) or a shell in a pseudo-terminal. A client that drops
//! leaves them running; one that comes back attaches and first gets what
//! they showed lately (the last MiB).

use std::collections::{HashMap, VecDeque};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use runo_link::frame::{Frame, MAX_PAYLOAD};
use runo_link::proto::{self, Down, Event, Kind};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

use super::feed::Feed;
use super::outbox::Outbox;
use super::trust::now;

/// What a session keeps of its output for clients that attach later.
const KEEP: usize = 1 << 20;
/// How long an ended session stays listed.
const ENDED_KEEP_SECS: i64 = 3600;

/// Where a session's output goes: a connection and its channel.
type Viewer = (u64, u32);

pub struct Session {
    pub id: u64,
    pub kind: Kind,
    pub started: i64,
    master: AsyncFd<OwnedFd>,
    output: Mutex<VecDeque<u8>>,
    viewers: Mutex<HashMap<Viewer, Outbox>>,
    /// The exit code once it ended (`Some(None)`: killed by a signal).
    ended: Mutex<Option<(i64, Option<i32>)>>,
    pid: Option<u32>,
}

/// How to start each kind.
pub struct Programs {
    /// This program, run as `tui` for the Agent and as `feed` for a feed.
    pub runo: PathBuf,
    pub config: Option<PathBuf>,
    /// Where a feed's Agent writes its errors.
    pub feed_log: PathBuf,
}

impl Programs {
    fn command(&self, kind: Kind) -> tokio::process::Command {
        match kind {
            Kind::Agent => {
                let mut command = tokio::process::Command::new(&self.runo);
                if let Some(config) = &self.config {
                    command.arg("--config").arg(config);
                }
                command.arg("tui");
                command
            }
            Kind::Shell => {
                let shell = std::env::var_os("SHELL")
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "/bin/sh".into());
                let mut command = tokio::process::Command::new(shell);
                command.arg("-l").env("RUNO_SHELL", "1");
                command
            }
            Kind::Feed => unreachable!("a feed is not a terminal"),
        }
    }
}

pub struct Sessions {
    sessions: Mutex<HashMap<u64, Arc<Session>>>,
    /// The Agent as an event stream; numbered with the terminals.
    feeds: Mutex<HashMap<u64, Arc<Feed>>>,
    next: AtomicU64,
    programs: Programs,
    /// Tests: what runs in place of `runo feed`.
    #[cfg(test)]
    pub feed_program: Mutex<Option<PathBuf>>,
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows.max(1),
        ws_col: cols.max(1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

pub(crate) fn set_size(fd: RawFd, cols: u16, rows: u16) {
    let size = winsize(cols, rows);
    // SAFETY: TIOCSWINSZ reads a winsize from a valid pointer.
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size) };
}

/// A pseudo-terminal of `cols` × `rows`: the master end (non-blocking)
/// and the slave end.
pub(crate) fn open_pty(cols: u16, rows: u16) -> Result<(OwnedFd, OwnedFd)> {
    let mut master = -1;
    let mut slave = -1;
    let size = winsize(cols, rows);
    // SAFETY: openpty writes two descriptors; the name and termios are unused.
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &size,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("opening a pseudo-terminal");
    }
    // SAFETY: both descriptors were just opened and are owned here.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    // SAFETY: plain fcntl calls on an open descriptor.
    unsafe {
        let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
    }
    Ok((master, slave))
}

/// Starts `command` on the slave end of a pseudo-terminal, as the leader
/// of a new session whose controlling terminal it is.
pub(crate) fn spawn_on(
    mut command: tokio::process::Command,
    slave: OwnedFd,
) -> Result<tokio::process::Child> {
    command
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave))
        .kill_on_drop(false);
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // The child holds the slave now; this end's copies close with `command`.
    Ok(command.spawn()?)
}

impl Session {
    pub fn info(&self) -> proto::Session {
        proto::Session {
            id: self.id,
            kind: self.kind,
            started: self.started,
            running: self.ended.lock().unwrap().is_none(),
            attached: self.viewers.lock().unwrap().len() as u32,
        }
    }

    /// Writes a client's keys.
    pub async fn write(&self, mut data: &[u8]) -> Result<()> {
        while !data.is_empty() {
            let mut guard = self.master.writable().await?;
            match guard.try_io(|fd| {
                // SAFETY: writes from a valid slice to an open descriptor.
                let n = unsafe { libc::write(fd.as_raw_fd(), data.as_ptr().cast(), data.len()) };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }) {
                Ok(Ok(n)) => data = &data[n..],
                Ok(Err(err)) => return Err(err.into()),
                Err(_would_block) => continue,
            }
        }
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        set_size(self.master.as_raw_fd(), cols, rows);
    }

    /// Shows the session on `channel` of connection `conn`: what it kept,
    /// then what comes.
    pub fn attach(&self, conn: u64, channel: u32, outbox: Outbox, cols: u16, rows: u16) {
        let kept: Vec<u8> = self.output.lock().unwrap().iter().copied().collect();
        for piece in kept.chunks(MAX_PAYLOAD) {
            outbox.send(Frame {
                channel,
                data: piece.to_vec(),
            });
        }
        let ended = *self.ended.lock().unwrap();
        if let Some((_, code)) = ended {
            outbox.event(Event::Exit { channel, code });
            return;
        }
        self.viewers.lock().unwrap().insert((conn, channel), outbox);
        // A full-screen program redraws on a size change: nudge it so the
        // new viewer gets a whole screen, not the tail of the old one.
        if cols > 1 {
            self.resize(cols - 1, rows);
        }
        self.resize(cols, rows);
    }

    pub fn detach(&self, conn: u64, channel: u32) -> bool {
        self.viewers
            .lock()
            .unwrap()
            .remove(&(conn, channel))
            .is_some()
    }

    pub fn detach_connection(&self, conn: u64) {
        self.viewers.lock().unwrap().retain(|(c, _), _| *c != conn);
    }

    pub fn shows_on(&self, conn: u64, channel: u32) -> bool {
        self.viewers.lock().unwrap().contains_key(&(conn, channel))
    }

    fn kill(&self) {
        if let Some(pid) = self.pid
            && self.ended.lock().unwrap().is_none()
        {
            // The session leader's process group: the shell and its children.
            // SAFETY: plain kill(2).
            unsafe {
                libc::kill(-(pid as i32), libc::SIGHUP);
                libc::kill(pid as i32, libc::SIGHUP);
            }
        }
    }

    fn output(&self, data: &[u8]) {
        {
            let mut kept = self.output.lock().unwrap();
            kept.extend(data);
            let over = kept.len().saturating_sub(KEEP);
            kept.drain(..over);
        }
        for ((_, channel), outbox) in self.viewers.lock().unwrap().iter() {
            outbox.send(Frame {
                channel: *channel,
                data: data.to_vec(),
            });
        }
    }

    fn end(&self, code: Option<i32>) {
        *self.ended.lock().unwrap() = Some((now(), code));
        for ((_, channel), outbox) in self.viewers.lock().unwrap().drain() {
            outbox.send(Frame {
                channel: runo_link::frame::CONTROL,
                data: serde_json::to_vec(&Down::Event(Event::Exit { channel, code })).unwrap(),
            });
        }
    }
}

impl Sessions {
    pub fn new(programs: Programs) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            feeds: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            programs,
            #[cfg(test)]
            feed_program: Mutex::new(None),
        }
    }

    pub fn get(&self, id: u64) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(&id).cloned()
    }

    /// The sessions, oldest first; ended ones go after an hour.
    pub fn list(&self) -> Vec<proto::Session> {
        let mut sessions = self.sessions.lock().unwrap();
        let at = now();
        sessions.retain(|_, s| match *s.ended.lock().unwrap() {
            Some((when, _)) => at - when < ENDED_KEEP_SECS,
            None => true,
        });
        let mut list: Vec<proto::Session> = sessions.values().map(|s| s.info()).collect();
        let mut feeds = self.feeds.lock().unwrap();
        feeds.retain(|_, f| f.ended_at().is_none_or(|when| at - when < ENDED_KEEP_SECS));
        list.extend(feeds.values().map(|f| f.info()));
        list.sort_by_key(|s| s.id);
        list
    }

    pub fn config(&self) -> Option<&std::path::Path> {
        self.programs.config.as_deref()
    }

    /// Starts the Agent as an event stream.
    pub fn open_feed(&self) -> Result<Arc<Feed>> {
        #[cfg(test)]
        let runo = self
            .feed_program
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| self.programs.runo.clone());
        #[cfg(not(test))]
        let runo = self.programs.runo.clone();
        let feed = Feed::start(
            self.next.fetch_add(1, Ordering::Relaxed),
            &runo,
            self.programs.config.as_deref(),
            &self.programs.feed_log,
        )?;
        self.feeds.lock().unwrap().insert(feed.id, feed.clone());
        Ok(feed)
    }

    pub fn feed(&self, id: u64) -> Option<Arc<Feed>> {
        self.feeds.lock().unwrap().get(&id).cloned()
    }

    /// The feed shown on `channel` of `conn`.
    pub fn feed_on(&self, conn: u64, channel: u32) -> Option<Arc<Feed>> {
        self.feeds
            .lock()
            .unwrap()
            .values()
            .find(|f| f.shows_on(conn, channel))
            .cloned()
    }

    /// The feeds someone is watching.
    pub fn watched_feeds(&self) -> Vec<Arc<Feed>> {
        self.feeds
            .lock()
            .unwrap()
            .values()
            .filter(|f| f.watched())
            .cloned()
            .collect()
    }

    /// The session shown on `channel` of `conn`.
    pub fn on_channel(&self, conn: u64, channel: u32) -> Option<Arc<Session>> {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .find(|s| s.shows_on(conn, channel))
            .cloned()
    }

    pub fn detach_connection(&self, conn: u64) {
        for session in self.sessions.lock().unwrap().values() {
            session.detach_connection(conn);
        }
        for feed in self.feeds.lock().unwrap().values() {
            feed.detach_connection(conn);
        }
    }

    pub fn kill(&self, id: u64) -> bool {
        if let Some(session) = self.sessions.lock().unwrap().remove(&id) {
            session.kill();
            return true;
        }
        match self.feeds.lock().unwrap().remove(&id) {
            Some(feed) => {
                feed.kill();
                true
            }
            None => false,
        }
    }

    pub fn kill_all(&self) {
        for (_, session) in self.sessions.lock().unwrap().drain() {
            session.kill();
        }
        for (_, feed) in self.feeds.lock().unwrap().drain() {
            feed.kill();
        }
    }

    /// Starts a `kind` terminal session of `cols` × `rows`.
    pub fn open(&self, kind: Kind, cols: u16, rows: u16) -> Result<Arc<Session>> {
        if kind == Kind::Feed {
            bail!("a feed is not a terminal");
        }
        let (master, slave) = open_pty(cols, rows)?;
        let mut command = self.programs.command(kind);
        command
            .env("TERM", "xterm-256color")
            .env("RUNO_REMOTE", "1");
        let mut child = spawn_on(command, slave).context("starting the session")?;
        let session = Arc::new(Session {
            id: self.next.fetch_add(1, Ordering::Relaxed),
            kind,
            started: now(),
            master: AsyncFd::with_interest(master, Interest::READABLE | Interest::WRITABLE)?,
            output: Mutex::new(VecDeque::new()),
            viewers: Mutex::new(HashMap::new()),
            ended: Mutex::new(None),
            pid: child.id(),
        });
        self.sessions
            .lock()
            .unwrap()
            .insert(session.id, session.clone());
        let reader = session.clone();
        tokio::spawn(async move {
            let mut buffer = vec![0u8; 16 * 1024];
            loop {
                let Ok(mut guard) = reader.master.readable().await else {
                    break;
                };
                match guard.try_io(|fd| {
                    // SAFETY: reads into a valid buffer from an open descriptor.
                    let n = unsafe {
                        libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len())
                    };
                    if n < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(n as usize)
                    }
                }) {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => reader.output(&buffer[..n]),
                    // EIO: every holder of the slave is gone.
                    Ok(Err(_)) => break,
                    Err(_would_block) => continue,
                }
            }
            let code = child.wait().await.ok().and_then(|status| status.code());
            reader.end(code);
        });
        Ok(session)
    }
}

/// Checks a terminal size from a client.
pub fn check_size(cols: u16, rows: u16) -> Result<()> {
    if cols == 0 || rows == 0 || cols > 1000 || rows > 1000 {
        bail!("bad terminal size {cols}×{rows}");
    }
    Ok(())
}
