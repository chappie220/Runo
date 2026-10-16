//! Feed sessions (Runo 1.0, 4.11): the Agent as an event stream. Each is
//! a `runo feed` process; what it writes is numbered and kept in a
//! `runo_link::feed::Log`, sent to the clients showing it one JSON event
//! per frame, and caught up for one that comes back. What clients say goes
//! to the process's standard input. The device's status joins the stream
//! every 2 seconds while someone watches.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use runo_link::feed::{Body, Event, Input, Log, Proxy, Status, Swap};
use runo_link::frame::Frame;
use runo_link::proto::{self, Kind};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use super::outbox::Outbox;
use super::trust::now;

/// How often the status joins the stream.
pub const STATUS_EVERY: Duration = Duration::from_secs(2);
/// A line longer than this from the Agent process is dropped.
const MAX_LINE: usize = 1 << 20;

type Viewer = (u64, u32);

pub struct Feed {
    pub id: u64,
    pub started: i64,
    input: mpsc::UnboundedSender<Input>,
    /// Held while sending, so events go out in order and a viewer that
    /// attaches misses none and gets none twice.
    log: Mutex<Log>,
    viewers: Mutex<HashMap<Viewer, Outbox>>,
    ended: Mutex<Option<(i64, Option<i32>)>>,
    pid: Option<u32>,
}

impl Feed {
    /// Starts `runo feed`.
    pub fn start(
        id: u64,
        runo: &Path,
        config: Option<&Path>,
        log_file: &Path,
    ) -> Result<Arc<Self>> {
        let mut command = tokio::process::Command::new(runo);
        if let Some(config) = config {
            command.arg("--config").arg(config);
        }
        command
            .arg("feed")
            .env("RUNO_REMOTE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(false);
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_file)
        {
            Ok(file) => command.stderr(file),
            Err(_) => command.stderr(Stdio::null()),
        };
        let mut child = command.spawn().context("starting the Agent")?;
        let stdin = child.stdin.take().context("no input to the Agent")?;
        let stdout = child.stdout.take().context("no output from the Agent")?;
        let (input, inputs) = mpsc::unbounded_channel();
        let feed = Arc::new(Self {
            id,
            started: now(),
            input,
            log: Mutex::new(Log::default()),
            viewers: Mutex::new(HashMap::new()),
            ended: Mutex::new(None),
            pid: child.id(),
        });
        tokio::spawn(write_inputs(stdin, inputs));
        let reader = feed.clone();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                line.clear();
                match lines.read_until(b'\n', &mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) if line.len() > MAX_LINE => continue,
                    Ok(_) => match serde_json::from_slice::<Body>(&line) {
                        // Status comes from here, not from the Agent.
                        Ok(Body::Status { .. }) => {}
                        Ok(body) => reader.push(body),
                        Err(err) => super::log(&format!("feed {}: {err}", reader.id)),
                    },
                }
            }
            let code = child.wait().await.ok().and_then(|status| status.code());
            reader.end(code);
        });
        Ok(feed)
    }

    pub fn info(&self) -> proto::Session {
        proto::Session {
            id: self.id,
            kind: Kind::Feed,
            started: self.started,
            running: self.ended.lock().unwrap().is_none(),
            attached: self.viewers.lock().unwrap().len() as u32,
        }
    }

    pub fn ended_at(&self) -> Option<i64> {
        self.ended.lock().unwrap().map(|(when, _)| when)
    }

    /// Numbers `body`, keeps it and sends it to every viewer.
    pub fn push(&self, body: Body) {
        let mut log = self.log.lock().unwrap();
        let Some(event) = log.push(body) else {
            return;
        };
        let data = serde_json::to_vec(&event).unwrap_or_default();
        for ((_, channel), outbox) in self.viewers.lock().unwrap().iter() {
            outbox.send(Frame {
                channel: *channel,
                data: data.clone(),
            });
        }
    }

    /// Shows the feed on `channel` of connection `conn`: what changed after
    /// event `since`, then what comes.
    pub fn attach(&self, conn: u64, channel: u32, outbox: Outbox, since: u64) {
        let log = self.log.lock().unwrap();
        for event in log.since(since) {
            outbox.send(frame(channel, &event));
        }
        let ended = *self.ended.lock().unwrap();
        if let Some((_, code)) = ended {
            outbox.event(proto::Event::Exit { channel, code });
            return;
        }
        self.viewers.lock().unwrap().insert((conn, channel), outbox);
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

    pub fn watched(&self) -> bool {
        !self.viewers.lock().unwrap().is_empty()
    }

    /// Passes what a client said on to the Agent.
    pub fn say(&self, input: Input) -> Result<()> {
        if self.ended.lock().unwrap().is_some() {
            anyhow::bail!("the Agent in session {} has ended", self.id);
        }
        self.input
            .send(input)
            .map_err(|_| anyhow::anyhow!("the Agent in session {} has ended", self.id))
    }

    pub fn kill(&self) {
        if let Some(pid) = self.pid
            && self.ended.lock().unwrap().is_none()
        {
            // SAFETY: plain kill(2).
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
    }

    fn end(&self, code: Option<i32>) {
        let _log = self.log.lock().unwrap();
        *self.ended.lock().unwrap() = Some((now(), code));
        for ((_, channel), outbox) in self.viewers.lock().unwrap().drain() {
            outbox.event(proto::Event::Exit { channel, code });
        }
    }
}

fn frame(channel: u32, event: &Event) -> Frame {
    Frame {
        channel,
        data: serde_json::to_vec(event).unwrap_or_default(),
    }
}

async fn write_inputs(
    mut stdin: tokio::process::ChildStdin,
    mut inputs: mpsc::UnboundedReceiver<Input>,
) {
    while let Some(input) = inputs.recv().await {
        let mut line = serde_json::to_vec(&input).unwrap_or_default();
        line.push(b'\n');
        if stdin.write_all(&line).await.is_err() || stdin.flush().await.is_err() {
            break;
        }
    }
}

/// Reads the device's status, each time comparing with the last reading
/// for the CPU use and the disk writes.
pub struct Sampler {
    state: PathBuf,
    config: PathBuf,
    cpu: Option<(u64, u64)>,
    written: Option<(u64, std::time::Instant)>,
    proxy: Option<(Proxy, std::time::Instant)>,
}

/// How long a proxy check is trusted.
const PROXY_CHECK: Duration = Duration::from_secs(30);

impl Sampler {
    pub fn new(state: &Path, config: Option<&Path>) -> Self {
        Self {
            state: state.to_owned(),
            config: config.map_or_else(|| state.join("config.toml"), Path::to_owned),
            cpu: None,
            written: None,
            proxy: None,
        }
    }

    pub async fn read(&mut self, pairing_open_until: Option<i64>) -> Status {
        let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
        let cpu = cpu_times(&read("/proc/stat"));
        let cpu_percent = match (self.cpu, cpu) {
            (Some((busy0, all0)), Some((busy, all))) if all > all0 => {
                (busy.saturating_sub(busy0)) as f32 * 100.0 / (all - all0) as f32
            }
            _ => 0.0,
        };
        self.cpu = cpu.or(self.cpu);
        let disk_write_bps = match device_of(&read("/proc/mounts"), &self.state)
            .and_then(|device| sectors_written(&read("/proc/diskstats"), &device))
        {
            Some(sectors) => {
                let bytes = sectors * 512;
                let at = std::time::Instant::now();
                let rate = match self.written {
                    Some((before, then)) if bytes >= before => {
                        let secs = at.duration_since(then).as_secs_f64();
                        if secs > 0.0 {
                            ((bytes - before) as f64 / secs) as u64
                        } else {
                            0
                        }
                    }
                    _ => 0,
                };
                self.written = Some((bytes, at));
                rate
            }
            None => 0,
        };
        Status {
            cpu_percent,
            temp_c: read("/sys/class/thermal/thermal_zone0/temp")
                .trim()
                .parse::<f32>()
                .ok()
                .map(|milli| milli / 1000.0),
            mem: memory(&read("/proc/meminfo")).unwrap_or_default(),
            swap: swaps(&read("/proc/swaps")),
            disk_write_bps,
            addresses: crate::system::addresses(),
            proxy: self.proxy().await,
            pairing_open_until,
            update: crate::sysupdate::status_line(&self.state, crate::i18n::current()),
        }
    }

    /// `network.proxy`, and whether it answers.
    async fn proxy(&mut self) -> Option<Proxy> {
        let address = crate::config::Config::load(&self.config)
            .ok()
            .and_then(|c| c.network.proxy)
            .filter(|p| !p.trim().is_empty())?;
        if let Some((kept, when)) = &self.proxy
            && kept.address == address
            && when.elapsed() < PROXY_CHECK
        {
            return Some(kept.clone());
        }
        let reachable = match host_port(&address) {
            Some(target) => matches!(
                tokio::time::timeout(
                    Duration::from_secs(1),
                    tokio::net::TcpStream::connect(target)
                )
                .await,
                Ok(Ok(_))
            ),
            None => false,
        };
        let proxy = Proxy { address, reachable };
        self.proxy = Some((proxy.clone(), std::time::Instant::now()));
        Some(proxy)
    }
}

/// `host:port` from a proxy URL such as `http://127.0.0.1:7890`.
fn host_port(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, rest)| rest);
    let host = rest.split('/').next()?;
    if host.is_empty() {
        return None;
    }
    if host
        .rsplit_once(':')
        .is_some_and(|(_, port)| port.parse::<u16>().is_ok())
    {
        return Some(host.to_owned());
    }
    let port = if url.starts_with("https://") { 443 } else { 80 };
    Some(format!("{host}:{port}"))
}

/// Busy and total CPU time from `/proc/stat`.
fn cpu_times(stat: &str) -> Option<(u64, u64)> {
    let fields: Vec<u64> = stat
        .lines()
        .next()?
        .strip_prefix("cpu ")?
        .split_whitespace()
        .filter_map(|f| f.parse().ok())
        .collect();
    let all: u64 = fields.iter().take(8).sum();
    // idle and iowait.
    let idle = fields.get(3)? + fields.get(4).unwrap_or(&0);
    Some((all - idle, all))
}

/// Used and total memory in bytes from `/proc/meminfo`.
fn memory(meminfo: &str) -> Option<(u64, u64)> {
    let field = |name: &str| {
        meminfo.lines().find_map(|line| {
            line.strip_prefix(name)?
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<u64>()
                .ok()
        })
    };
    let total = field("MemTotal:")?;
    let available = field("MemAvailable:")?;
    Some((total.saturating_sub(available) * 1024, total * 1024))
}

/// The swap areas from `/proc/swaps`, with zram's compressed size.
fn swaps(text: &str) -> Vec<Swap> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let device = (*fields.first()?).to_owned();
            let size = fields.get(2)?.parse::<u64>().ok()? * 1024;
            let used = fields.get(3)?.parse::<u64>().ok()? * 1024;
            let compressed = device
                .strip_prefix("/dev/")
                .filter(|name| name.starts_with("zram"))
                .and_then(|name| std::fs::read_to_string(format!("/sys/block/{name}/mm_stat")).ok())
                .and_then(|stat| stat.split_whitespace().nth(2)?.parse().ok());
            Some(Swap {
                device,
                size,
                used,
                compressed,
            })
        })
        .collect()
}

/// The block device (`mmcblk0p4`) of the file system holding `path`.
fn device_of(mounts: &str, path: &Path) -> Option<String> {
    mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let device = fields.next()?.strip_prefix("/dev/")?;
            let point = fields.next()?;
            path.starts_with(point)
                .then(|| (point.len(), device.to_owned()))
        })
        .max_by_key(|(length, _)| *length)
        .map(|(_, device)| device)
}

/// Sectors written to `device`, from `/proc/diskstats`.
fn sectors_written(diskstats: &str, device: &str) -> Option<u64> {
    diskstats.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        (fields.get(2) == Some(&device)).then(|| fields.get(9)?.parse().ok())?
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_read_from_proc() {
        assert_eq!(
            cpu_times("cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 1 2 3\n"),
            Some((150, 1000))
        );
        assert_eq!(
            memory("MemTotal:        8000 kB\nMemFree:  100 kB\nMemAvailable:    6000 kB\n"),
            Some((2000 * 1024, 8000 * 1024))
        );
        let swap = swaps(
            "Filename\tType\tSize\tUsed\tPriority\n/dev/zram0  partition\t3984380\t1024\t-2\n",
        );
        assert_eq!(swap.len(), 1);
        assert_eq!(swap[0].device, "/dev/zram0");
        assert_eq!(swap[0].used, 1024 * 1024);
        let mounts = "/dev/root / ext4 ro 0 0\n/dev/mmcblk0p4 /data ext4 rw,noatime 0 0\ntmpfs /tmp tmpfs rw 0 0\n";
        assert_eq!(
            device_of(mounts, Path::new("/data/runo")),
            Some("mmcblk0p4".into())
        );
        assert_eq!(device_of(mounts, Path::new("/root")), Some("root".into()));
        let diskstats = " 179       0 mmcblk0 1 2 3 4 5 6 7 8 0 0 0\n 179       4 mmcblk0p4 10 0 300 20 40 0 1234 50 0 60 70\n";
        assert_eq!(sectors_written(diskstats, "mmcblk0p4"), Some(1234));
        assert_eq!(sectors_written(diskstats, "sda"), None);
    }

    #[test]
    fn a_proxy_url_gives_where_to_connect() {
        assert_eq!(
            host_port("http://127.0.0.1:7890").as_deref(),
            Some("127.0.0.1:7890")
        );
        assert_eq!(
            host_port("socks5://user:pw@[::1]:1080/").as_deref(),
            Some("[::1]:1080")
        );
        assert_eq!(
            host_port("https://proxy.lan").as_deref(),
            Some("proxy.lan:443")
        );
        assert_eq!(host_port("http://"), None);
    }
}
