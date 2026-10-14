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
