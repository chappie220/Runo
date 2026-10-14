//! `runo daemon …` on the device: run the daemon, set the device password,
//! open the pairing window and watch the codes, list and revoke clients.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Result, bail};
use runo_link::proto::{self, PORT};

use super::trust::{MIN_PASSWORD, Trust};
use super::{DaemonState, Local, control_request, dir, identity};
use crate::i18n::{Lang, Tr, tr};

const PASSWORD_PROMPT: Tr = tr(
    "New device password (at least {} characters): ",
    "新的设备密码（至少 {} 个字符）：",
);
const PASSWORD_AGAIN: Tr = tr("Once more: ", "再输入一次：");
const PASSWORD_DIFFERENT: Tr = tr("The two passwords differ.", "两次输入的密码不一样。");
const PASSWORD_SET: Tr = tr(
    "Device password set. Clients ask for it when they pair.",
    "设备密码已设置。客户端配对时需要输入它。",
);
const PASSWORD_FIRST: Tr = tr(
    "Pairing needs a device password; set one now.",
    "配对需要设备密码；现在设置一个。",
);
const FIRST_START: Tr = tr(
    "This device has no device password yet. Clients on this network give it once, when they pair; press Enter to skip (set it later under Manage).",
    "本设备还没有设备密码。本网络中的客户端配对时需要输入一次；直接回车跳过（以后可在管理中设置）。",
);
const DDNS_OFF: Tr = tr(
    "Dynamic DNS is not set up: runo daemon ddns set <cloudflare|aliyun|dnspod> <domain>",
    "动态域名未设置：runo daemon ddns set <cloudflare|aliyun|dnspod> <域名>",
);
const DDNS_LINE: Tr = tr("{} at {}", "{}（{}）");
const DDNS_POINTS: Tr = tr("points at {} since {}", "指向 {}，更新于 {}");
const DDNS_PENDING: Tr = tr("not updated yet", "尚未更新");
const DDNS_FAILED: Tr = tr("last update failed: {}", "上次更新失败：{}");
const DDNS_NO_IPV6: Tr = tr(
    "This network gives the device no public IPv6 address, so it can be reached only on the local network.",
    "这个网络没有给设备分配公网 IPv6 地址，因此只能在局域网内访问。",
);
const DDNS_SECRET: Tr = tr("{} for {}: ", "{}（{}）：");
const DDNS_SAVED: Tr = tr(
    "Saved. {} now points at {}; runod keeps it up to date.",
    "已保存。{} 现在指向 {}；runod 会持续更新。",
);
const DDNS_SAVED_LATER: Tr = tr(
    "Saved; runod points {} at the device once it has a public IPv6 address.",
    "已保存；设备有公网 IPv6 地址后，runod 会把 {} 指向它。",
);
const DDNS_REMOVED: Tr = tr(
    "Dynamic DNS is off and the credentials are deleted.",
    "动态域名已关闭，凭据已删除。",
);
const NOT_RUNNING: Tr = tr(
    "The daemon is not running: start it with `runo daemon` (or install it with `runo daemon install`).",
    "管理守护进程没有运行：用 `runo daemon` 启动（或用 `runo daemon install` 安装成服务）。",
);
const WINDOW_OPEN: Tr = tr(
    "Pairing window open until {}. On the client, pick this device ({}) and compare the code shown there with the one here. Ctrl+C closes the window.",
    "配对窗口已打开，到 {} 关闭。在客户端选择这台设备（{}），核对那里显示的确认码和这里的是否一致。Ctrl+C 关闭窗口。",
);
const CODE: Tr = tr(
    "Client {} at {} is pairing. Code: {}",
    "客户端 {}（{}）正在配对。确认码：{}",
);
const PAIRED: Tr = tr("Paired: {} ({}).", "已配对：{}（{}）。");
const WINDOW_CLOSED: Tr = tr("Pairing window closed.", "配对窗口已关闭。");
const NO_CLIENTS: Tr = tr(
    "No trusted clients. Pair one with `runo daemon pair`.",
    "还没有可信客户端。用 `runo daemon pair` 配对一个。",
);
const CLIENT_LINE: Tr = tr(
    "{}  {}  paired {}  last seen {}{}",
    "{}  {}  配对于 {}  最近连接 {}{}",
);
const CONNECTED: Tr = tr("  (connected)", "  （已连接）");
const REVOKED: Tr = tr(
    "Revoked {} ({}); its connections were closed.",
    "已撤销 {}（{}）；它的连接已断开。",
);
const REVOKED_OFFLINE: Tr = tr("Revoked {} ({}).", "已撤销 {}（{}）。");
const STATUS: Tr = tr(
    "Device {} ({})\nDaemon: {}\nPort: {}\nPairing window: {}\nDevice password: {}\nTrusted clients: {}",
    "设备 {}（{}）\n守护进程：{}\n端口：{}\n配对窗口：{}\n设备密码：{}\n可信客户端：{}",
);
const RUNNING: Tr = tr("running", "运行中");
const STOPPED: Tr = tr("not running", "未运行");
const OPEN_UNTIL: Tr = tr("open until {}", "开着，到 {} 关闭");
const CLOSED: Tr = tr("closed", "关闭");
const SET: Tr = tr("set", "已设置");
const NOT_SET: Tr = tr(
    "not set (runo daemon password)",
    "未设置（runo daemon password）",
);
const NEVER: Tr = tr("never", "从未");

#[cfg(test)]
pub const ALL: &[Tr] = &[
    PASSWORD_PROMPT,
    PASSWORD_AGAIN,
    PASSWORD_DIFFERENT,
    PASSWORD_SET,
    PASSWORD_FIRST,
    FIRST_START,
    DDNS_OFF,
    DDNS_LINE,
    DDNS_POINTS,
    DDNS_PENDING,
    DDNS_FAILED,
    DDNS_NO_IPV6,
    DDNS_SECRET,
    DDNS_SAVED,
    DDNS_SAVED_LATER,
    DDNS_REMOVED,
    NOT_RUNNING,
    WINDOW_OPEN,
    CODE,
    PAIRED,
    WINDOW_CLOSED,
    NO_CLIENTS,
    CLIENT_LINE,
    CONNECTED,
    REVOKED,
    REVOKED_OFFLINE,
    STATUS,
    RUNNING,
    STOPPED,
    OPEN_UNTIL,
    CLOSED,
    SET,
    NOT_SET,
    NEVER,
];

/// A line typed without echo (or read from a pipe).
pub fn read_secret(prompt: &str) -> Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    if std::io::stdin().is_terminal() {
        let _echo = crate::setup::EchoOff::new();
        std::io::stdin().lock().read_line(&mut line)?;
        eprintln!();
    } else {
        std::io::stdin().lock().read_line(&mut line)?;
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

pub fn set_password(state: &Path) -> Result<()> {
    let password = read_secret(&PASSWORD_PROMPT.with(&[&MIN_PASSWORD.to_string()]))?;
    if std::io::stdin().is_terminal() {
        let again = read_secret(PASSWORD_AGAIN.now())?;
        if again != password {
            bail!("{}", PASSWORD_DIFFERENT.now());
        }
    }
    Trust::new(&dir(state)).set_password(&password)?;
    println!("{}", PASSWORD_SET.now());
    Ok(())
}

/// On the Runo system's console: asks for the device password the first
/// time, until one is set; Enter skips.
pub fn first_password(state: &Path) -> Result<()> {
    let trust = Trust::new(&dir(state));
    if !crate::config::runo_system() || trust.has_password() || !std::io::stdin().is_terminal() {
        return Ok(());
    }
    println!("{}\n", FIRST_START.now());
    loop {
        let password = read_secret(&PASSWORD_PROMPT.with(&[&MIN_PASSWORD.to_string()]))?;
        if password.is_empty() {
            return Ok(());
        }
        if read_secret(PASSWORD_AGAIN.now())? != password {
            eprintln!("{}", PASSWORD_DIFFERENT.now());
            continue;
        }
        match trust.set_password(&password) {
            Ok(()) => {
                println!("{}", PASSWORD_SET.now());
                return Ok(());
            }
            Err(err) => eprintln!("{err:#}"),
        }
    }
}

/// The dynamic DNS lines of the status page, in `lang`.
pub fn ddns_lines(state: &Path, lang: Lang) -> Vec<String> {
    use super::ddns;
    let dir = dir(state);
    let Some(config) = ddns::load(&dir) else {
        return vec![DDNS_OFF.get(lang).to_owned()];
    };
    let mut lines = vec![DDNS_LINE.fill(
        lang,
        &[
            &config.domain,
            ddns::Provider::NAMES[config.provider as usize],
        ],
    )];
    let status = ddns::status(&dir);
    match (&status.address, status.updated) {
        (Some(address), Some(at)) => lines.push(DDNS_POINTS.fill(lang, &[address, &date(at)])),
        _ => lines.push(DDNS_PENDING.get(lang).to_owned()),
    }
    if let Some(error) = &status.error {
        lines.push(DDNS_FAILED.fill(lang, &[error]));
    }
    if ddns::live_address().is_none() {
        lines.push(DDNS_NO_IPV6.get(lang).to_owned());
    }
    lines
}

pub fn ddns_show(state: &Path) -> Result<()> {
    for line in ddns_lines(state, crate::i18n::current()) {
        println!("{line}");
    }
    Ok(())
}

pub async fn ddns_set(
    state: &Path,
    provider: &str,
    domain: &str,
    zone: Option<&str>,
) -> Result<()> {
    use super::ddns::{self, Config, Provider};
    let Some(provider) = Provider::parse(provider) else {
        bail!("the provider is one of {}", Provider::NAMES.join(", "));
    };
    // Checked before asking for anything.
    Config::new(
        provider,
        domain,
        zone,
        provider.secrets().iter().map(|_| "x".to_owned()).collect(),
    )?;
    let mut secrets = Vec::new();
    for name in provider.secrets() {
        secrets.push(read_secret(
            &DDNS_SECRET.with(&[name, Provider::NAMES[provider as usize]]),
        )?);
    }
    let config = Config::new(provider, domain, zone, secrets)?;
    let dir = dir(state);
    if ddns::live_address().is_none() {
        ddns::save(&dir, &config)?;
        println!("{}", DDNS_SAVED_LATER.with(&[&config.domain]));
        println!("{}", DDNS_NO_IPV6.now());
        return Ok(());
    }
    // Saved only once the provider took it, so wrong credentials are not kept.
    let address = ddns::update_now(&dir, &config).await?;
    ddns::save(&dir, &config)?;
    ddns::record(&dir, address);
    println!(
        "{}",
        DDNS_SAVED.with(&[&config.domain, &address.to_string()])
    );
    Ok(())
}

pub async fn ddns_update(state: &Path) -> Result<()> {
    let dir = dir(state);
    let Some(config) = super::ddns::load(&dir) else {
        bail!("{}", DDNS_OFF.now());
    };
    let address = super::ddns::update_now(&dir, &config).await?;
    println!(
        "{}",
        DDNS_SAVED.with(&[&config.domain, &address.to_string()])
    );
    Ok(())
}

pub fn ddns_off(state: &Path) -> Result<()> {
    super::ddns::remove(&dir(state))?;
    println!("{}", DDNS_REMOVED.now());
    Ok(())
}

pub fn date(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

fn clock(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}

async fn running(state: &Path) -> Result<DaemonState> {
    control_request(state, Local::State)
        .await
        .map_err(|_| anyhow::anyhow!("{}", NOT_RUNNING.now()))
}

/// Opens the window and shows codes until a client pairs or it closes.
pub async fn pair(state: &Path) -> Result<()> {
    let before = running(state).await?;
    if !before.password {
        eprintln!("{}", PASSWORD_FIRST.now());
        set_password(state)?;
    }
    let opened: proto::PairingState = control_request(state, Local::Pairing { open: true }).await?;
    println!(
        "{}",
        WINDOW_OPEN.with(&[&clock(opened.until.unwrap_or_default()), &before.id])
    );
    let known: Vec<String> = before.clients.iter().map(|c| c.id.clone()).collect();
    let mut shown: Vec<String> = vec![];
    let watch = async {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let now = running(state).await?;
            for pending in &now.pending {
                let key = format!("{}{}", pending.client, pending.code);
                if !shown.contains(&key) {
                    println!(
                        "{}",
                        CODE.with(&[&pending.client, &pending.address, &spaced(&pending.code)])
                    );
                    shown.push(key);
                }
            }
            if let Some(new) = now.clients.iter().find(|c| !known.contains(&c.id)) {
                println!("{}", PAIRED.with(&[&new.name, &new.id]));
                return Ok::<_, anyhow::Error>(());
            }
            if !now.pairing.open {
                println!("{}", WINDOW_CLOSED.now());
                return Ok(());
            }
        }
    };
    tokio::select! {
        result = watch => result,
        _ = tokio::signal::ctrl_c() => {
            let _: proto::PairingState = control_request(state, Local::Pairing { open: false }).await?;
            println!("{}", WINDOW_CLOSED.now());
            Ok(())
        }
    }
}

/// `123456` as `123 456`, easier to compare.
pub fn spaced(code: &str) -> String {
    if code.len() == 6 {
        format!("{} {}", &code[..3], &code[3..])
    } else {
        code.to_owned()
    }
}

pub fn client_lines(clients: &[proto::Client], lang: Lang) -> Vec<String> {
    if clients.is_empty() {
        return vec![NO_CLIENTS.get(lang).to_owned()];
    }
    clients
        .iter()
        .map(|c| {
            CLIENT_LINE.fill(
                lang,
                &[
                    &c.id,
                    &c.name,
                    &date(c.added),
                    &c.last_seen
                        .map(date)
                        .unwrap_or_else(|| NEVER.get(lang).into()),
                    if c.connected { CONNECTED.get(lang) } else { "" },
                ],
            )
        })
        .collect()
}

fn offline_clients(state: &Path) -> Result<Vec<proto::Client>> {
    Ok(Trust::new(&dir(state))
        .clients()?
        .into_iter()
        .map(|c| proto::Client {
            id: c.id,
            name: c.name,
            added: c.added,
            last_seen: c.last_seen,
            connected: false,
            this: false,
        })
        .collect())
}

pub async fn clients(state: &Path) -> Result<()> {
    let clients = match control_request::<DaemonState>(state, Local::State).await {
        Ok(running) => running.clients,
        Err(_) => offline_clients(state)?,
    };
    for line in client_lines(&clients, crate::i18n::current()) {
        println!("{line}");
    }
    Ok(())
}

pub async fn revoke(state: &Path, client: &str) -> Result<()> {
    if super::is_running(state) {
        let gone: proto::Client = control_request(
            state,
            Local::Revoke {
                client: client.to_owned(),
            },
        )
        .await?;
        println!("{}", REVOKED.with(&[&gone.name, &gone.id]));
    } else {
        let gone = Trust::new(&dir(state)).revoke(client)?;
        println!("{}", REVOKED_OFFLINE.with(&[&gone.name, &gone.id]));
    }
    Ok(())
}

pub async fn status(state: &Path) -> Result<()> {
    let text = match control_request::<DaemonState>(state, Local::State).await {
        Ok(running) => STATUS.with(&[
            &running.id,
            &running.name,
            RUNNING.now(),
            &running.port.to_string(),
            &match running.pairing.until {
                Some(until) if running.pairing.open => OPEN_UNTIL.with(&[&clock(until)]),
                _ => CLOSED.now().to_owned(),
            },
            if running.password {
                SET.now()
            } else {
                NOT_SET.now()
            },
            &running.clients.len().to_string(),
        ]),
        Err(_) => {
            let trust = Trust::new(&dir(state));
            STATUS.with(&[
                &identity(state)?.id(),
                &super::device_name(),
                STOPPED.now(),
                &PORT.to_string(),
                CLOSED.now(),
                if trust.has_password() {
                    SET.now()
                } else {
                    NOT_SET.now()
                },
                &trust.clients()?.len().to_string(),
            ])
        }
    };
    println!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_text_is_translated() {
        crate::i18n::assert_complete(super::ALL);
    }

    #[test]
    fn codes_are_split_for_reading() {
        assert_eq!(super::spaced("012345"), "012 345");
        assert_eq!(super::spaced("12"), "12");
    }
}
