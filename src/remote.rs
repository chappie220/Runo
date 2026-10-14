//! `runo remote …`: this computer as a management client of Runo devices
//! (Runo 1.0, 4.10, for the terminal): find devices on the LAN, pair, and
//! then use a paired device's Agent, shell, status, logs and files.
//!
//! The client's identity and its paired devices live in
//! `<state>/remote/`; a device is known by the key pinned at pairing, and
//! the addresses kept for it are only hints (the LAN is searched again
//! when none answers).

use std::io::{IsTerminal, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use runo_link::client::{Device, Home, Incoming, Link};
use runo_link::mdns::{self, Found};
use runo_link::proto::{self, Kind, PORT, Request};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::daemon::cli::{client_lines, date, read_secret, spaced};
use crate::i18n::{Tr, tr};

const SEARCH: Duration = Duration::from_secs(3);
/// Ctrl+]: leaves a session running and returns here.
const DETACH: u8 = 0x1d;

const NONE_FOUND: Tr = tr(
    "No Runo device answered on this network.",
    "这个网络里没有 Runo 设备应答。",
);
const FOUND_LINE: Tr = tr("{}  {}  {}{}{}", "{}  {}  {}{}{}");
const PAIRING_OPEN: Tr = tr("  (pairing window open)", "  （配对窗口开着）");
const ALREADY_PAIRED: Tr = tr("  (paired)", "  （已配对）");
const WHICH: Tr = tr(
    "More than one device has its pairing window open; name one: runo remote pair <ID or name>.",
    "不止一台设备开着配对窗口；请指定：runo remote pair <ID 或名称>。",
);
const NONE_PAIRING: Tr = tr(
    "No device on this network has its pairing window open. Open it on the device (Pairing in the management view, or `runo daemon pair`).",
    "这个网络里没有设备开着配对窗口。请在设备上打开（管理界面的“配对”，或 `runo daemon pair`）。",
);
const NOT_FOUND: Tr = tr("No device {} on this network.", "这个网络里没有设备 {}。");
const CODE: Tr = tr(
    "Pairing with {} ({}).\nCode: {}\nThe device shows a code too. Is it the same? [y/N] ",
    "正在和 {}（{}）配对。\n确认码：{}\n设备上也显示了一个确认码。两者一样吗？[y/N] ",
);
const MISMATCH: Tr = tr(
    "Not paired: the codes must match. If they differ, something between this computer and the device may be listening in.",
    "没有配对：确认码必须一致。不一致时，可能有人在这台电脑和设备之间窃听。",
);
const PASSWORD: Tr = tr("Device password: ", "设备密码：");
const LOGIN: Tr = tr(
    "{} is reached from outside its local network: log in with the device password.",
    "正在从 {} 所在局域网之外连接：请输入设备密码登录。",
);
const PAIRED: Tr = tr(
    "Paired with {} ({}). Connect with: runo remote connect {}",
    "已和 {}（{}）配对。连接：runo remote connect {}",
);
const WAS_PAIRED: Tr = tr(
    "This device already trusts this computer; it is remembered again.",
    "这台设备已经信任这台电脑；已重新记下它。",
);
const NO_DEVICES: Tr = tr(
    "No paired devices. Find one with `runo remote discover`, then `runo remote pair`.",
    "还没有配对的设备。先用 `runo remote discover` 查找，再 `runo remote pair`。",
);
const DEVICE_LINE: Tr = tr("{}  {}  paired {}  {}", "{}  {}  配对于 {}  {}");
const NOT_TRUSTED: Tr = tr(
    "{} no longer trusts this computer (it was revoked, or the device was reset). Pair again: runo remote pair {}",
    "{} 不再信任这台电脑（已被撤销，或设备被重置）。重新配对：runo remote pair {}",
);
const UNREACHABLE: Tr = tr("Cannot reach {}: {}", "连不上 {}：{}");
const DETACHED: Tr = tr(
    "\r\nLeft session {} running. Back to it: runo remote attach {} {}",
    "\r\n会话 {} 仍在运行。回到它：runo remote attach {} {}",
);
const ENDED: Tr = tr("\r\nSession ended.", "\r\n会话已结束。");
const CONNECTED: Tr = tr(
    "Connected to {}. Ctrl+] leaves the session running and returns here.",
    "已连接 {}。Ctrl+] 离开会话（会话继续运行）并回到这里。",
);
const NO_SESSIONS: Tr = tr("No sessions.", "没有会话。");
const SESSION_LINE: Tr = tr(
    "{}  {}  started {}  {}  {} attached",
    "{}  {}  开始于 {}  {}  {} 个连接在看",
);
const RUNNING: Tr = tr("running", "运行中");
const ENDED_STATE: Tr = tr("ended", "已结束");
const SENT: Tr = tr("Sent {} bytes to {}.", "已发送 {} 字节到 {}。");
const FETCHED: Tr = tr("Fetched {} bytes into {}.", "已取回 {} 字节到 {}。");
const FORGOTTEN: Tr = tr("Forgot {}.", "已忘记 {}。");
const WINDOW: Tr = tr("Pairing window on {}: {}", "{} 的配对窗口：{}");
const OPEN: Tr = tr("open", "已打开");
const CLOSED: Tr = tr("closed", "已关闭");
const REVOKED: Tr = tr("Revoked {} ({}).", "已撤销 {}（{}）。");

#[cfg(test)]
pub const ALL: &[Tr] = &[
    NONE_FOUND,
    FOUND_LINE,
    PAIRING_OPEN,
    ALREADY_PAIRED,
    WHICH,
    NONE_PAIRING,
    NOT_FOUND,
    CODE,
    MISMATCH,
    PASSWORD,
    LOGIN,
    PAIRED,
    WAS_PAIRED,
    NO_DEVICES,
    DEVICE_LINE,
    NOT_TRUSTED,
    UNREACHABLE,
    DETACHED,
    ENDED,
    CONNECTED,
    NO_SESSIONS,
    SESSION_LINE,
    RUNNING,
    ENDED_STATE,
    SENT,
    FETCHED,
    FORGOTTEN,
    WINDOW,
    OPEN,
    CLOSED,
    REVOKED,
];

pub fn home(state: &Path) -> Home {
    Home::new(&state.join("remote"))
}

/// Lists the devices on the LAN.
pub async fn discover(state: &Path) -> Result<()> {
    let paired: Vec<String> = home(state).devices()?.into_iter().map(|d| d.id).collect();
    let found = mdns::browse(SEARCH).await?;
    if found.is_empty() {
        println!("{}", NONE_FOUND.now());
    }
    for device in found {
        let addresses: Vec<String> = device.addresses.iter().map(|a| a.to_string()).collect();
        println!(
            "{}",
            FOUND_LINE.with(&[
                &device.id,
                &device.name,
                &addresses.join(", "),
                if device.pairing {
                    PAIRING_OPEN.now()
                } else {
                    ""
                },
                if paired.contains(&device.id) {
                    ALREADY_PAIRED.now()
                } else {
                    ""
                },
            ])
        );
    }
    Ok(())
}

/// `target` as addresses to try: `host:port`, an address, or a device
/// found on the LAN by ID or name.
async fn resolve(target: Option<&str>) -> Result<(Vec<SocketAddr>, Option<Found>)> {
    if let Some(target) = target {
        if let Ok(address) = target.parse::<SocketAddr>() {
            return Ok((vec![address], None));
        }
        if let Ok(ip) = target.parse::<std::net::IpAddr>() {
            return Ok((vec![SocketAddr::new(ip, PORT)], None));
        }
        if target.contains(':') && !target.contains('-') {
            let addresses: Vec<SocketAddr> = tokio::net::lookup_host(target).await?.collect();
            return Ok((addresses, None));
        }
    }
    let found = mdns::browse(SEARCH).await?;
    let device = match target {
        Some(target) => {
            let lower = target.to_lowercase();
            found
                .into_iter()
                .find(|d| {
                    d.id == lower
                        || d.name == target
                        || (lower.len() >= 4 && d.id.starts_with(&lower))
                })
                .with_context(|| NOT_FOUND.with(&[target]))?
        }
        None => {
            let mut open: Vec<Found> = found.into_iter().filter(|d| d.pairing).collect();
            match open.len() {
                0 => bail!("{}", NONE_PAIRING.now()),
                1 => open.remove(0),
                _ => bail!("{}", WHICH.now()),
            }
        }
    };
    Ok((device.addresses.clone(), Some(device)))
}

fn ask_yes(prompt: &str) -> Result<bool> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_lowercase().as_str(),
        "y" | "yes" | "是"
    ))
}

pub async fn pair(state: &Path, target: Option<&str>) -> Result<()> {
    let home = home(state);
    let identity = home.identity()?;
    let (addresses, _) = resolve(target).await?;
    let link = Link::connect(&addresses, &identity, None).await?;
    let welcome = link.welcome().clone();
    let remember = |link: &Link| {
        home.remember(Device {
            id: welcome.device.clone(),
            name: welcome.name.clone(),
            key: runo_link::hex::encode(&link.device_key()),
            addresses: vec![link.address().to_string()],
            domain: None,
            paired: chrono::Utc::now().timestamp(),
        })
    };
    if welcome.trusted {
        remember(&link)?;
        println!("{}", WAS_PAIRED.now());
        return Ok(());
    }
    let code = link.begin_pairing().await?;
    if !ask_yes(&CODE.with(&[&welcome.name, &welcome.device, &spaced(&code)]))? {
        bail!("{}", MISMATCH.now());
    }
    let name = crate::daemon::device_name();
    let mut tries = 0;
    loop {
        let password = read_secret(PASSWORD.now())?;
        match link.finish_pairing(&password, &name).await {
            Ok(_) => break,
            Err(err) => {
                tries += 1;
                let text = err.to_string();
                if tries >= 3
                    || !text.starts_with("wrong device password")
                    || text.contains("closed")
                {
                    return Err(err);
                }
                eprintln!("{text}");
            }
        }
    }
    remember(&link)?;
    println!(
        "{}",
        PAIRED.with(&[&welcome.name, &welcome.device, &welcome.name])
    );
    Ok(())
}

pub fn devices(state: &Path) -> Result<()> {
    let devices = home(state).devices()?;
    if devices.is_empty() {
        println!("{}", NO_DEVICES.now());
    }
    for device in devices {
        println!(
            "{}",
            DEVICE_LINE.with(&[
                &device.id,
                &device.name,
                &date(device.paired),
                &device.addresses.join(", "),
            ])
        );
    }
    Ok(())
}

pub fn forget(state: &Path, device: &str) -> Result<()> {
    let home = home(state);
    let device = home.find(device)?;
    home.forget(&device.id)?;
    println!("{}", FORGOTTEN.with(&[&device.name]));
    Ok(())
}

/// A connection to the paired device `query` names, as a trusted client:
/// its last addresses first, then wherever it answers on the LAN.
pub async fn connect(state: &Path, query: &str) -> Result<Link> {
    let home = home(state);
    let device = home.find(query)?;
    let identity = home.identity()?;
    let key = device.key().context("the kept device key is malformed")?;
    let mut tried = device.socket_addresses();
    if let Some(domain) = &device.domain
        && let Ok(found) = tokio::net::lookup_host((domain.as_str(), PORT)).await
    {
        tried.extend(found);
    }
    let first = match Link::connect(&tried, &identity, Some(key)).await {
        Ok(link) => Ok(link),
        Err(err) => {
            let found = mdns::browse(SEARCH).await.unwrap_or_default();
            match found.into_iter().find(|d| d.id == device.id) {
                Some(found) => Link::connect(&found.addresses, &identity, Some(key)).await,
                None => Err(err),
            }
        }
    };
    let link = first.map_err(|err| {
        anyhow::anyhow!("{}", UNREACHABLE.with(&[&device.name, &format!("{err:#}")]))
    })?;
    if !link.welcome().trusted {
        bail!("{}", NOT_TRUSTED.with(&[&device.name, &device.id]));
    }
    if link.needs_login() {
        if !std::io::stdin().is_terminal() {
            bail!("{}", LOGIN.with(&[&device.name]));
        }
        eprintln!("{}", LOGIN.with(&[&device.name]));
        let password = read_secret(PASSWORD.now())?;
        link.login(&password).await?;
    }
    home.reached(&device.id, link.address())?;
    Ok(link)
}

fn terminal_size() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((80, 24))
}

/// The device's Agent: the one already running, or a new one.
pub async fn agent(state: &Path, device: &str) -> Result<()> {
    let link = connect(state, device).await?;
    let sessions: Vec<proto::Session> = link.call(Request::Sessions).await?;
    let (cols, rows) = terminal_size();
    let running = sessions
        .iter()
        .rev()
        .find(|s| s.kind == Kind::Agent && s.running);
    let (session, channel) = match running {
        Some(session) => link.attach(session.id, cols, rows).await?,
        None => link.open(Kind::Agent, cols, rows).await?,
    };
    terminal(device, session.id, channel, link.welcome().name.clone()).await
}

pub async fn shell(state: &Path, device: &str) -> Result<()> {
    let link = connect(state, device).await?;
    let (cols, rows) = terminal_size();
    let (session, channel) = link.open(Kind::Shell, cols, rows).await?;
    terminal(device, session.id, channel, link.welcome().name.clone()).await
}

pub async fn attach(state: &Path, device: &str, session: u64) -> Result<()> {
    let link = connect(state, device).await?;
    let (cols, rows) = terminal_size();
    let (session, channel) = link.attach(session, cols, rows).await?;
    terminal(device, session.id, channel, link.welcome().name.clone()).await
}

/// Puts the terminal in raw mode for the length of a session.
struct Raw;

impl Raw {
    fn new() -> Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Shows `channel` here until the session ends or Ctrl+] is pressed.
async fn terminal(
    device: &str,
    session: u64,
    mut channel: runo_link::client::Channel,
    name: String,
) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("this needs a terminal");
    }
    eprintln!("{}", CONNECTED.with(&[&name]));
    let link = channel.link().clone();
    let id = channel.channel;
    let _raw = Raw::new()?;
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut resized =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut buffer = [0u8; 4096];
    loop {
        tokio::select! {
            incoming = channel.recv() => match incoming {
                Some(Incoming::Data(data)) => {
                    stdout.write_all(&data).await?;
                    stdout.flush().await?;
                }
                Some(Incoming::Exit(_)) => {
                    eprint!("{}\r\n", ENDED.now());
                    return Ok(());
                }
                Some(_) => {}
                None => bail!("{}", link.closed().unwrap_or_default()),
            },
            read = stdin.read(&mut buffer) => {
                let read = read?;
                if read == 0 {
                    return Ok(());
                }
                let input = &buffer[..read];
                if let Some(at) = input.iter().position(|b| *b == DETACH) {
                    channel.send(&input[..at])?;
                    let _ = link.detach(id).await;
                    eprint!("{}\r\n", DETACHED.with(&[&session.to_string(), device, &session.to_string()]));
                    return Ok(());
                }
                channel.send(input)?;
            }
            _ = resized.recv() => {
                let (cols, rows) = terminal_size();
                let _ = link.resize(id, cols, rows).await;
            }
        }
    }
}

pub async fn sessions(state: &Path, device: &str) -> Result<()> {
    let link = connect(state, device).await?;
    let sessions: Vec<proto::Session> = link.call(Request::Sessions).await?;
    if sessions.is_empty() {
        println!("{}", NO_SESSIONS.now());
    }
    for s in sessions {
        let kind = match s.kind {
            Kind::Agent => "agent",
            Kind::Shell => "shell",
        };
        println!(
            "{}",
            SESSION_LINE.with(&[
                &s.id.to_string(),
                kind,
                &date(s.started),
                if s.running {
                    RUNNING.now()
                } else {
                    ENDED_STATE.now()
                },
                &s.attached.to_string(),
            ])
        );
    }
    Ok(())
}

pub async fn kill(state: &Path, device: &str, session: u64) -> Result<()> {
    let link = connect(state, device).await?;
    let () = link.call(Request::Kill { session }).await?;
    Ok(())
}

fn lang_code() -> String {
    match crate::i18n::current() {
        crate::i18n::Lang::Zh => "zh".into(),
        crate::i18n::Lang::En => "en".into(),
    }
}

pub async fn status(state: &Path, device: &str) -> Result<()> {
    let link = connect(state, device).await?;
    let text: proto::Text = link
        .call(Request::Status {
            lang: Some(lang_code()),
        })
        .await?;
    println!("{}", text.text);
    Ok(())
}

pub async fn logs(state: &Path, device: &str, lines: Option<usize>) -> Result<()> {
    let link = connect(state, device).await?;
    let text: proto::Text = link
        .call(Request::Logs {
            lang: Some(lang_code()),
            lines,
        })
        .await?;
    println!("{}", text.text);
    Ok(())
}

pub async fn get(state: &Path, device: &str, remote: &str, local: Option<PathBuf>) -> Result<()> {
    let link = connect(state, device).await?;
    let (info, bytes) = link.get_file(remote).await?;
    let local = local.unwrap_or_else(|| {
        PathBuf::from(
            Path::new(remote)
                .file_name()
                .unwrap_or_else(|| "download".as_ref()),
        )
    });
    std::fs::write(&local, &bytes).with_context(|| format!("writing {}", local.display()))?;
    println!(
        "{}",
        FETCHED.with(&[&info.size.to_string(), &local.display().to_string()])
    );
    Ok(())
}

pub async fn put(state: &Path, device: &str, local: &Path, remote: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let link = connect(state, device).await?;
    let file = tokio::fs::File::open(local)
        .await
        .with_context(|| format!("opening {}", local.display()))?;
    let meta = file.metadata().await?;
    // A directory as the target gets the file's own name.
    let remote = if remote.ends_with('/') {
        format!(
            "{remote}{}",
            local.file_name().unwrap_or_default().to_string_lossy()
        )
    } else {
        remote.to_owned()
    };
    link.put_file(
        &remote,
        meta.len(),
        Some(meta.permissions().mode() & 0o777),
        file,
    )
    .await?;
    println!("{}", SENT.with(&[&meta.len().to_string(), &remote]));
    Ok(())
}

pub async fn clients(state: &Path, device: &str) -> Result<()> {
    let link = connect(state, device).await?;
    let clients: Vec<proto::Client> = link.call(Request::Clients).await?;
    for line in client_lines(&clients, crate::i18n::current()) {
        println!("{line}");
    }
    Ok(())
}

pub async fn revoke(state: &Path, device: &str, client: &str) -> Result<()> {
    let link = connect(state, device).await?;
    let gone: proto::Client = link
        .call(Request::Revoke {
            client: client.to_owned(),
        })
        .await?;
    println!("{}", REVOKED.with(&[&gone.name, &gone.id]));
    Ok(())
}

pub async fn pairing(state: &Path, device: &str, open: bool) -> Result<()> {
    let link = connect(state, device).await?;
    let window: proto::PairingState = link.call(Request::Pairing { open }).await?;
    println!(
        "{}",
        WINDOW.with(&[
            &link.welcome().name,
            if window.open {
                OPEN.now()
            } else {
                CLOSED.now()
            }
        ])
    );
    Ok(())
}

/// Keeps change `id` (or the one waiting) on the device, or puts it back.
pub async fn confirm(state: &Path, device: &str, id: Option<u64>, keep: bool) -> Result<()> {
    let link = connect(state, device).await?;
    let change = match id {
        Some(id) => id,
        None => {
            let waiting: Vec<proto::Unconfirmed> = link.call(Request::Unconfirmed).await?;
            match waiting.first() {
                Some(change) => change.id,
                None => bail!(
                    "no change is waiting for confirmation on {}",
                    link.welcome().name
                ),
            }
        }
    };
    let report: proto::Text = link.call(Request::Confirm { change, keep }).await?;
    print!("{}", report.text);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_text_is_translated() {
        crate::i18n::assert_complete(super::ALL);
    }
}
