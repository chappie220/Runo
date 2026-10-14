//! The management view: what the window shows when the model cannot be
//! used (no network, a refused key or sign-in, no credit, the provider down),
//! and on `/manage`. Nothing in it needs the model: checking the model
//! again, changing its connection, the system's status, the logs and the
//! shell all work while it is down.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::Config;
use crate::i18n::{Lang, Tr, tr};
use crate::onboard::{Catalog, KeyCheck};

pub(super) const TITLE: Tr = tr("Management", "管理");
pub(super) const HINTS: Tr = tr(
    "↑/↓ choose · Enter run · 1–6 · Tab input · Esc back to the chat",
    "↑/↓ 选择 · 回车执行 · 1–6 · Tab 输入框 · Esc 返回对话",
);
const MODEL: Tr = tr("Model {}: {}", "模型 {}：{}");
const UNCHECKED: Tr = tr("not checked", "未检查");
const CHECKING: Tr = tr("checking…", "检查中……");
const UP: Tr = tr("available ({})", "可用（{}）");
const DOWN: Tr = tr("unavailable: {}", "不可用：{}");
const DOWN_NOTE: Tr = tr(
    "The model cannot be used right now. Everything below works without it; messages typed below are still sent and come back here if it is still down.",
    "模型现在用不了。下面这些不需要模型也能用；在下方输入的消息仍会发送，模型还不可用时会回到这里。",
);
const NO_MODEL: Tr = tr("no model is set", "还没设置模型");
const NO_KEY: Tr = tr("no API key is set", "还没设置 API 密钥");
const NOT_SIGNED_IN: Tr = tr("not signed in to ChatGPT: {}", "没有登录 ChatGPT：{}");
const SIGNED_IN: Tr = tr("signed in to ChatGPT", "已登录 ChatGPT");
const KEY_OK: Tr = tr("key accepted", "密钥有效");
const KEY_OK_CREDIT: Tr = tr("key accepted, {}", "密钥有效，{}");
const REACHABLE: Tr = tr(
    "server reachable, key not checked: {}",
    "服务器可达，密钥未检查：{}",
);
const ANSWERED: Tr = tr("answered in {}s", "{} 秒内回答");

const ITEM_CHECK: Tr = tr("Check the model again", "重新检查模型");
const ITEM_MODEL: Tr = tr("Change the model connection", "修改模型连接");
const ITEM_STATUS: Tr = tr("System status", "系统状态");
const ITEM_LOGS: Tr = tr("Logs", "日志");
const ITEM_SHELL: Tr = tr("Shell (or type !shell)", "Shell（也可输入 !shell）");
const ITEM_BACK: Tr = tr("Back to the chat", "返回对话");

const S_DEVICE: Tr = tr("Device", "设备");
const S_HOST: Tr = tr("Host name", "主机名");
const S_ADDRESSES: Tr = tr("Addresses", "地址");
const S_NO_ADDRESS: Tr = tr("none (no network)", "无（没有网络）");
const S_UPTIME: Tr = tr("Up", "已运行");
const S_LOAD: Tr = tr("Load", "负载");
const S_MEMORY: Tr = tr("Memory", "内存");
const S_MEMORY_VALUE: Tr = tr("{} free of {}", "可用 {} / 共 {}");
const S_DISK: Tr = tr("Disk ({})", "磁盘（{}）");
const S_TEMPERATURE: Tr = tr("Temperature", "温度");
const S_GATEWAY: Tr = tr("Gateway", "Gateway");
const S_GATEWAY_NONE: Tr = tr(
    "no service set up (runo service install)",
    "没有安装服务（runo service install）",
);
const UNKNOWN: Tr = tr("unknown", "未知");
const LOG_EMPTY: Tr = tr("(empty or missing)", "（空或不存在）");
const LOG_UNREADABLE: Tr = tr("(cannot read: {})", "（无法读取：{}）");

#[cfg(test)]
pub(super) const ALL: &[Tr] = &[
    TITLE,
    HINTS,
    MODEL,
    UNCHECKED,
    CHECKING,
    UP,
    DOWN,
    DOWN_NOTE,
    NO_MODEL,
    NO_KEY,
    NOT_SIGNED_IN,
    SIGNED_IN,
    KEY_OK,
    KEY_OK_CREDIT,
    REACHABLE,
    ANSWERED,
    ITEM_CHECK,
    ITEM_MODEL,
    ITEM_STATUS,
    ITEM_LOGS,
    ITEM_SHELL,
    ITEM_BACK,
    S_DEVICE,
    S_HOST,
    S_ADDRESSES,
    S_NO_ADDRESS,
    S_UPTIME,
    S_LOAD,
    S_MEMORY,
    S_MEMORY_VALUE,
    S_DISK,
    S_TEMPERATURE,
    S_GATEWAY,
    S_GATEWAY_NONE,
    UNKNOWN,
    LOG_EMPTY,
    LOG_UNREADABLE,
];

/// Lines of each log shown.
const LOG_LINES: usize = 40;

/// What is known about the model.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Health {
    Unchecked,
    Checking,
    Up(String),
    Down(String),
}

impl Health {
    pub(super) fn is_down(&self) -> bool {
        matches!(self, Self::Down(_))
    }

    /// One line: "Model x/y: unavailable: …".
    pub(super) fn line(&self, lang: Lang, model: &str) -> String {
        let state = match self {
            Self::Unchecked => UNCHECKED.get(lang).to_owned(),
            Self::Checking => CHECKING.get(lang).to_owned(),
            Self::Up(how) => UP.fill(lang, &[how]),
            Self::Down(why) => DOWN.fill(lang, &[why]),
        };
        MODEL.fill(lang, &[model, &state])
    }

    pub(super) fn note(lang: Lang) -> &'static str {
        DOWN_NOTE.get(lang)
    }

    pub(super) fn answered(lang: Lang, took: Duration) -> Self {
        Self::Up(ANSWERED.fill(lang, &[&format!("{:.1}", took.as_secs_f64())]))
    }
}

/// A check that costs nothing: the settings, the ChatGPT sign-in, or the
/// key at the model's server (`/key`). It finds no network, a refused key
/// and missing settings; a wrong model id shows only when a message fails.
pub(super) async fn probe(config: &Config, state: &Path, lang: Lang) -> Health {
    if config.model_id().is_err() {
        return Health::Down(NO_MODEL.get(lang).to_owned());
    }
    if config.chatgpt() {
        return match crate::chatgpt::read(&crate::chatgpt::auth_path(state)) {
            Ok(_) => Health::Up(SIGNED_IN.get(lang).to_owned()),
            Err(err) => Health::Down(NOT_SIGNED_IN.fill(lang, &[&format!("{err:#}")])),
        };
    }
    let Ok(key) = config.model_key() else {
        return Health::Down(NO_KEY.get(lang).to_owned());
    };
    let Ok(catalog) = crate::onboard::OpenRouter::new(&config.model.base_url) else {
        return Health::Unchecked;
    };
    from_key_check(catalog.check_key(&key).await, lang)
}

fn from_key_check(check: KeyCheck, lang: Lang) -> Health {
    match check {
        KeyCheck::Valid(Some(credit)) => Health::Up(KEY_OK_CREDIT.fill(lang, &[&credit])),
        KeyCheck::Valid(None) => Health::Up(KEY_OK.get(lang).to_owned()),
        KeyCheck::Invalid(why) => Health::Down(why),
        // An answer, even an error, means the server is there; servers
        // other than OpenRouter need not have `/key`.
        KeyCheck::Unknown(why) if why.starts_with("HTTP ") => {
            Health::Up(REACHABLE.fill(lang, &[&why]))
        }
        KeyCheck::Unknown(why) => Health::Down(why),
    }
}

/// The menu, in order; 1–6 pick them too.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Item {
    Check,
    Model,
    Status,
    Logs,
    Shell,
    Back,
}

pub(super) const ITEMS: &[Item] = &[
    Item::Check,
    Item::Model,
    Item::Status,
    Item::Logs,
    Item::Shell,
    Item::Back,
];

impl Item {
    pub(super) fn label(self, lang: Lang) -> &'static str {
        match self {
            Self::Check => ITEM_CHECK,
            Self::Model => ITEM_MODEL,
            Self::Status => ITEM_STATUS,
            Self::Logs => ITEM_LOGS,
            Self::Shell => ITEM_SHELL,
            Self::Back => ITEM_BACK,
        }
        .get(lang)
    }
}

/// The view's state while it is open.
#[derive(Debug, Default)]
pub(super) struct Manage {
    pub selected: usize,
    /// What the last item showed: a title and its lines.
    pub detail: Option<(String, Vec<String>)>,
}

/// The system's status, a line each, read from /proc and /sys.
pub(super) fn status(state: &Path, lang: Lang) -> Vec<String> {
    let read = |path: &str| std::fs::read_to_string(path).ok();
    let unknown = || UNKNOWN.get(lang).to_owned();
    let row = |label: Tr, value: String| format!("{}: {value}", label.get(lang));
    let mut rows = Vec::new();
    let device = read("/proc/device-tree/model")
        .map(|m| m.trim_end_matches('\0').trim().to_owned())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(unknown);
    rows.push(row(S_DEVICE, device));
    let host = read("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_owned())
        .unwrap_or_else(unknown);
    rows.push(row(S_HOST, host));
    let addresses = addresses();
    rows.push(row(
        S_ADDRESSES,
        if addresses.is_empty() {
            S_NO_ADDRESS.get(lang).to_owned()
        } else {
            addresses.join(", ")
        },
    ));
    let uptime = read("/proc/uptime")
        .and_then(|u| u.split_whitespace().next()?.parse::<f64>().ok())
        .map_or_else(unknown, |s| duration(s as u64));
    rows.push(row(S_UPTIME, uptime));
    let load = read("/proc/loadavg")
        .map(|l| l.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
        .unwrap_or_else(unknown);
    rows.push(row(S_LOAD, load));
    let memory = read("/proc/meminfo")
        .and_then(|m| meminfo(&m))
        .map_or_else(unknown, |(total, free)| {
            S_MEMORY_VALUE.fill(lang, &[&bytes(free * 1024), &bytes(total * 1024)])
        });
    rows.push(row(S_MEMORY, memory));
    let disk = disk(state).map_or_else(unknown, |(total, free)| {
        S_MEMORY_VALUE.fill(lang, &[&bytes(free), &bytes(total)])
    });
    rows.push(format!(
        "{}: {disk}",
        S_DISK.fill(lang, &[&state.display().to_string()])
    ));
    if let Some(celsius) =
        read("/sys/class/thermal/thermal_zone0/temp").and_then(|t| temperature(&t))
    {
        rows.push(row(S_TEMPERATURE, celsius));
    }
    let gateway = crate::service::describe().unwrap_or_else(|| S_GATEWAY_NONE.get(lang).to_owned());
    rows.push(row(S_GATEWAY, gateway));
    rows
}

/// The window's own log and the Gateway's, the last lines of each.
pub(super) fn logs(state: &Path, lang: Lang) -> Vec<String> {
    let mut files = vec![state.join("tui.log")];
    if let Some(gateway) = crate::service::log_file()
        && !files.contains(&gateway)
    {
        files.push(gateway);
    }
    let mut rows = Vec::new();
    for file in files {
        rows.push(format!("── {} ──", file.display()));
        rows.extend(tail(&file, LOG_LINES, lang));
    }
    rows
}

fn tail(path: &PathBuf, lines: usize, lang: Lang) -> Vec<String> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            let all: Vec<&str> = text.lines().collect();
            if all.is_empty() {
                return vec![LOG_EMPTY.get(lang).to_owned()];
            }
            all[all.len().saturating_sub(lines)..]
                .iter()
                .map(|l| (*l).to_owned())
                .collect()
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            vec![LOG_EMPTY.get(lang).to_owned()]
        }
        Err(err) => vec![LOG_UNREADABLE.fill(lang, &[&err.to_string()])],
    }
}

/// "eth0 192.168.1.20", every interface but loopback.
fn addresses() -> Vec<String> {
    let mut out = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list`, freed below; each entry is read only
    // while the list lives, and only the family its sa_family names.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut at = list;
        while !at.is_null() {
            let entry = &*at;
            at = entry.ifa_next;
            if entry.ifa_addr.is_null() || entry.ifa_flags & libc::IFF_LOOPBACK as u32 != 0 {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(entry.ifa_name).to_string_lossy();
            let address = match i32::from((*entry.ifa_addr).sa_family) {
                libc::AF_INET => {
                    let a = &*(entry.ifa_addr as *const libc::sockaddr_in);
                    std::net::IpAddr::from(u32::from_be(a.sin_addr.s_addr).to_be_bytes())
                }
                libc::AF_INET6 => {
                    let a = &*(entry.ifa_addr as *const libc::sockaddr_in6);
                    std::net::IpAddr::from(a.sin6_addr.s6_addr)
                }
                _ => continue,
            };
            out.push(format!("{name} {address}"));
        }
        libc::freeifaddrs(list);
    }
    out
}

/// Total and free bytes of the file system holding `path`.
fn disk(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs writes into `stat`, which lives across the call.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let unit = stat.f_frsize as u64;
    Some((stat.f_blocks as u64 * unit, stat.f_bavail as u64 * unit))
}

/// MemTotal and MemAvailable, in kB.
fn meminfo(text: &str) -> Option<(u64, u64)> {
    let field = |name: &str| {
        text.lines().find_map(|l| {
            l.strip_prefix(name)?
                .trim()
                .strip_suffix("kB")?
                .trim()
                .parse()
                .ok()
        })
    };
    Some((field("MemTotal:")?, field("MemAvailable:")?))
}

/// "52.3 °C" from millidegrees.
fn temperature(text: &str) -> Option<String> {
    let milli: f64 = text.trim().parse().ok()?;
    Some(format!("{:.1} °C", milli / 1000.0))
}

/// "3d 4h 05m", "4h 05m", "12m".
fn duration(seconds: u64) -> String {
    let (days, hours, minutes) = (
        seconds / 86_400,
        seconds % 86_400 / 3600,
        seconds % 3600 / 60,
    );
    match (days, hours) {
        (0, 0) => format!("{minutes}m"),
        (0, _) => format!("{hours}h {minutes:02}m"),
        _ => format!("{days}d {hours}h {minutes:02}m"),
    }
}

/// "1.9 GB", "512 MB".
fn bytes(n: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let mb = n as f64 / MB;
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{mb:.0} MB")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_proc_and_sys_values() {
        let text = "MemTotal:        8063116 kB\nMemFree:  100 kB\nMemAvailable:    6123456 kB\n";
        assert_eq!(meminfo(text), Some((8_063_116, 6_123_456)));
        assert_eq!(meminfo("MemTotal: 1 kB\n"), None);
        assert_eq!(temperature("52312\n").as_deref(), Some("52.3 °C"));
        assert_eq!(temperature("n/a"), None);
        assert_eq!(duration(59), "0m");
        assert_eq!(duration(4 * 3600 + 5 * 60), "4h 05m");
        assert_eq!(duration(3 * 86_400 + 4 * 3600 + 5 * 60), "3d 4h 05m");
        assert_eq!(bytes(512 * 1024 * 1024), "512 MB");
        assert_eq!(bytes(2040 * 1024 * 1024), "2.0 GB");
    }

    #[test]
    fn a_free_check_tells_a_refused_key_from_an_unreachable_server() {
        let en = Lang::En;
        assert_eq!(
            from_key_check(KeyCheck::Valid(Some("$3 left".into())), en),
            Health::Up("key accepted, $3 left".into())
        );
        assert_eq!(
            from_key_check(KeyCheck::Invalid("HTTP 401 no auth".into()), en),
            Health::Down("HTTP 401 no auth".into())
        );
        assert!(!from_key_check(KeyCheck::Unknown("HTTP 404 Not Found".into()), en).is_down());
        assert!(from_key_check(KeyCheck::Unknown("connection refused".into()), en).is_down());
    }

    #[tokio::test]
    async fn missing_settings_are_down_without_asking_anyone() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.model.model = String::new();
        assert_eq!(
            probe(&config, dir.path(), Lang::En).await,
            Health::Down("no model is set".into())
        );
    }

    #[test]
    fn status_and_logs_say_something_for_every_row() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tui.log"), "one\ntwo\n").unwrap();
        let rows = status(dir.path(), Lang::En);
        assert!(rows.iter().any(|r| r.starts_with("Memory: ")), "{rows:?}");
        assert!(rows.iter().any(|r| r.starts_with("Disk (")), "{rows:?}");
        let logs = logs(dir.path(), Lang::En);
        assert!(logs[0].contains("tui.log"));
        assert_eq!(&logs[1..3], ["one", "two"]);
    }
}
