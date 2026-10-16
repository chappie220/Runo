//! The device the agent runs on, for setting it up: facts read from /proc,
//! /sys and /etc, the services of its init system, and checks that a step
//! worked. Only what BusyBox systems also have: no package manager, no GNU
//! tools, nothing beyond `/etc/init.d` scripts on a plain BusyBox init.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// Programs whose presence decides how the device can be set up.
const COMMANDS: &[&str] = &[
    "wpa_supplicant",
    "wpa_cli",
    "iw",
    "ip",
    "udhcpc",
    "udhcpc6",
    "dhcpcd",
    "nmcli",
    "ntpd",
    "chronyd",
    "hwclock",
    "nft",
    "systemctl",
    "rc-service",
    "rc-update",
    "apt-get",
    "apk",
];

/// Files that say which program manages the network.
const NETWORK_MANAGERS: &[(&str, &str)] = &[
    ("NetworkManager", "etc/NetworkManager/NetworkManager.conf"),
    ("dhcpcd", "etc/dhcpcd.conf"),
    ("ifupdown", "etc/network/interfaces"),
    ("systemd-networkd", "etc/systemd/network"),
    ("wpa_supplicant", "etc/wpa_supplicant/wpa_supplicant.conf"),
    ("wpa_supplicant", "etc/wpa_supplicant.conf"),
];

/// Where Wi-Fi networks are kept: the usual place, and the Runo system's.
const WPA_CONFS: &[&str] = &[
    "etc/wpa_supplicant/wpa_supplicant.conf",
    "etc/wpa_supplicant.conf",
];

/// What the agent needs to know about the Runo system image.
const RUNO_SYSTEM: &str = "runo system: this is Runo's own image. The root file system is read-only; \
what may change lives on /data and is linked from /etc: /etc/wpa_supplicant.conf (Wi-Fi, applied \
by restarting S40network), /etc/timezone (a zone name such as Asia/Shanghai, applied by restarting \
S48time) and /etc/hostname. Wired network is DHCP (S40network); runod (S50runod) serves paired \
clients; the Gateway is S60gateway, turned on with `runo gateway install`. Nothing else under /etc \
can be changed, and nothing can be installed.";

/// Services listed at most; the rest are counted.
const MAX_SERVICES: usize = 80;
/// How long a check may take.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// The init system, which decides how services are run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Init {
    Systemd,
    Openrc,
    /// BusyBox init or SysV: scripts in /etc/init.d, nothing else.
    Scripts,
}

impl Init {
    fn name(self) -> &'static str {
        match self {
            Init::Systemd => "systemd",
            Init::Openrc => "OpenRC",
            Init::Scripts => "init scripts (BusyBox init or SysV)",
        }
    }
}

/// The file system the facts are read from: `/`, or a copy in tests.
pub struct Host {
    root: PathBuf,
}

impl Host {
    pub fn live() -> Self {
        Self { root: "/".into() }
    }

    #[cfg(test)]
    fn at(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
        }
    }

    fn path(&self, path: &str) -> PathBuf {
        self.root.join(path)
    }

    fn read(&self, path: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.path(path)).ok()?;
        // The device tree ends its strings with NUL.
        let text = text.trim_matches(|c: char| c == '\0' || c.is_whitespace());
        (!text.is_empty()).then(|| text.to_owned())
    }

    fn exists(&self, path: &str) -> bool {
        self.path(path).exists()
    }

    /// The board, as the device tree names it.
    pub fn model(&self) -> Option<String> {
        self.read("proc/device-tree/model")
    }

    /// Whether this is a Raspberry Pi, the device Runo sets itself up on.
    pub fn is_raspberry_pi(&self) -> bool {
        self.model().is_some_and(|m| m.starts_with("Raspberry Pi"))
    }

    pub fn init(&self) -> Init {
        if self.exists("run/systemd/system") {
            Init::Systemd
        } else if self.exists("run/openrc") || self.exists("sbin/openrc-run") {
            Init::Openrc
        } else {
            Init::Scripts
        }
    }

    fn os(&self) -> Option<String> {
        let text = self.read("etc/os-release")?;
        let field = |key: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
                .map(|v| v.trim_matches('"').to_owned())
        };
        field("PRETTY_NAME").or_else(|| field("NAME"))
    }

    fn timezone(&self) -> Option<String> {
        if let Some(zone) = self.read("etc/timezone") {
            return Some(zone);
        }
        let link = std::fs::read_link(self.path("etc/localtime")).ok()?;
        let link = link.to_string_lossy();
        link.split_once("zoneinfo/")
            .map(|(_, zone)| zone.to_owned())
            .or_else(|| Some(link.into_owned()))
    }

    /// Network interfaces with their kind and state, loopback left out.
    fn interfaces(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.path("sys/class/net"))
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "lo")
            .collect();
        names.sort();
        names
            .into_iter()
            .map(|name| {
                let dir = format!("sys/class/net/{name}");
                let kind = if self.exists(&format!("{dir}/wireless"))
                    || self.exists(&format!("{dir}/phy80211"))
                {
                    "wifi"
                } else if self.exists(&format!("{dir}/device")) {
                    "ethernet"
                } else {
                    "virtual"
                };
                let state = self
                    .read(&format!("{dir}/operstate"))
                    .unwrap_or_else(|| "unknown".into());
                format!("{name} ({kind}, {state})")
            })
            .collect()
    }

    /// The interface of the IPv4 default route, from /proc/net/route.
    fn default_route(&self) -> Option<String> {
        let text = self.read("proc/net/route")?;
        text.lines().skip(1).find_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            (fields.get(1) == Some(&"00000000") && fields.get(7) == Some(&"00000000"))
                .then(|| fields[0].to_owned())
        })
    }

    /// The interface of the IPv6 default route, from /proc/net/ipv6_route.
    fn default_route6(&self) -> Option<String> {
        let text = self.read("proc/net/ipv6_route")?;
        text.lines().find_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let default = fields.first() == Some(&"00000000000000000000000000000000")
                && fields.get(1) == Some(&"00");
            let device = fields.get(9).filter(|d| **d != "lo")?;
            default.then(|| (*device).to_owned())
        })
    }

    fn nameservers(&self) -> Vec<String> {
        self.read("etc/resolv.conf")
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().strip_prefix("nameserver"))
            .map(|s| s.trim().to_owned())
            .collect()
    }

    /// Networks in the Wi‑Fi configuration, by SSID; never the keys.
    fn wifi_networks(&self) -> Option<(&'static str, Vec<String>)> {
        WPA_CONFS.iter().find_map(|path| {
            let text = self.read(path)?;
            Some((
                *path,
                text.lines()
                    .filter_map(|l| l.trim().strip_prefix("ssid="))
                    .map(|s| s.trim().to_owned())
                    .collect(),
            ))
        })
    }

    fn services(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.path("etc/init.d"))
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.') && n != "README" && n != "functions")
            .collect();
        names.sort();
        names
    }

    /// Everything the agent needs to plan the set-up, as text.
    pub fn inspect(&self) -> String {
        let unknown = || "unknown".to_owned();
        let mut out = String::new();
        let model = self.model();
        let _ = writeln!(
            out,
            "device: {}",
            model.as_deref().unwrap_or("unknown (no device tree model)")
        );
        if !self.is_raspberry_pi() {
            out.push_str("  (not a Raspberry Pi: Runo sets up Raspberry Pi devices only; change nothing here unless the user asks)\n");
        }
        let _ = writeln!(out, "os: {}", self.os().unwrap_or_else(unknown));
        if self.exists("etc/runo-system") {
            let _ = writeln!(out, "{RUNO_SYSTEM}");
        }
        let _ = writeln!(
            out,
            "kernel: {} ({})",
            self.read("proc/sys/kernel/osrelease")
                .unwrap_or_else(unknown),
            std::env::consts::ARCH
        );
        let init = self.init();
        let _ = writeln!(out, "init: {}", init.name());
        if crate::elevate::is_root() {
            out.push_str("user: root\n");
        } else {
            // SAFETY: geteuid has no preconditions.
            let uid = unsafe { libc::geteuid() };
            let _ = writeln!(
                out,
                "user: uid {uid}, not root: system files and services cannot be changed; ask the user to run Runo as root to set up the device"
            );
        }
        let _ = writeln!(
            out,
            "hostname: {}",
            self.read("proc/sys/kernel/hostname")
                .unwrap_or_else(unknown)
        );
        let _ = writeln!(
            out,
            "timezone: {}",
            self.timezone().unwrap_or_else(|| "not set (UTC)".into())
        );
        let now = chrono::Utc::now();
        let _ = writeln!(
            out,
            "clock (UTC): {}{}",
            now.format("%Y-%m-%d %H:%M"),
            if chrono::Datelike::year(&now) < 2025 {
                " (looks unset: no network time yet)"
            } else {
                ""
            }
        );

        out.push_str("network interfaces:\n");
        let interfaces = self.interfaces();
        if interfaces.is_empty() {
            out.push_str("  none found\n");
        }
        for interface in interfaces {
            let _ = writeln!(out, "  {interface}");
        }
        if self.root == Path::new("/") {
            for address in addresses() {
                let _ = writeln!(out, "  address {address}");
            }
        }
        let _ = writeln!(
            out,
            "default route: IPv4 {}, IPv6 {}",
            self.default_route().as_deref().unwrap_or("none"),
            self.default_route6().as_deref().unwrap_or("none")
        );
        let servers = self.nameservers();
        let _ = writeln!(
            out,
            "dns servers: {}",
            if servers.is_empty() {
                "none".to_owned()
            } else {
                servers.join(", ")
            }
        );
        let managers: Vec<&str> = NETWORK_MANAGERS
            .iter()
            .filter(|(_, path)| self.exists(path))
            .map(|(name, _)| *name)
            .collect();
        let _ = writeln!(
            out,
            "network configured by: {}",
            if managers.is_empty() {
                "nothing found".to_owned()
            } else {
                managers.join(", ")
            }
        );
        if let Some((path, networks)) = self.wifi_networks() {
            let _ = writeln!(
                out,
                "wifi networks in /{path}: {}",
                if networks.is_empty() {
                    "none".to_owned()
                } else {
                    networks.join(", ")
                }
            );
        }

        let (present, missing): (Vec<&str>, Vec<&str>) = COMMANDS
            .iter()
            .partition(|c| self.root != Path::new("/") || crate::elevate::on_path(c));
        let _ = writeln!(out, "commands present: {}", present.join(" "));
        if !missing.is_empty() {
            let _ = writeln!(out, "commands missing: {}", missing.join(" "));
        }
        if !present.contains(&"apt-get") && !present.contains(&"apk") {
            out.push_str("no package manager: nothing can be installed; use what is here\n");
        }

        let services = self.services();
        let _ = write!(out, "services in /etc/init.d ({}):", services.len());
        for name in services.iter().take(MAX_SERVICES) {
            let _ = write!(out, " {name}");
        }
        if services.len() > MAX_SERVICES {
            let _ = write!(out, " … {} more", services.len() - MAX_SERVICES);
        }
        out.push('\n');
        if init == Init::Systemd {
            out.push_str("  (systemd units are not listed; ask system_service for one by name)\n");
        }
        let _ = writeln!(
            out,
            "runo gateway: {}",
            crate::service::describe().unwrap_or_else(|| "not installed".into())
        );
        out
    }
}

/// Addresses of every interface but loopback, as `name address`.
pub fn addresses() -> Vec<String> {
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

/// What can be done to a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Status,
    Start,
    Stop,
    Restart,
    Enable,
    Disable,
}

impl Action {
    pub const NAMES: &[&str] = &["status", "start", "stop", "restart", "enable", "disable"];

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text.trim() {
            "status" => Action::Status,
            "start" => Action::Start,
            "stop" => Action::Stop,
            "restart" => Action::Restart,
            "enable" => Action::Enable,
            "disable" => Action::Disable,
            _ => return None,
        })
    }

    fn word(self) -> &'static str {
        Self::NAMES[self as usize]
    }

    /// Whether it changes the system, and so needs a permission.
    pub fn changes(self) -> bool {
        self != Action::Status
    }
}

/// A service name as init systems take it; nothing that reaches a path or a flag.
pub fn valid_service(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '-'))
}

/// The commands that do `action` on `name` under `init`, run in order;
/// `Err` when this init system cannot do it.
pub fn service_commands(
    init: Init,
    name: &str,
    action: Action,
) -> Result<Vec<Vec<String>>, String> {
    let words = |w: &[&str]| w.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let script = format!("/etc/init.d/{name}");
    Ok(match (init, action) {
        (Init::Systemd, Action::Status) => vec![
            words(&["systemctl", "is-active", name]),
            words(&["systemctl", "is-enabled", name]),
        ],
        (Init::Systemd, action) => vec![words(&["systemctl", action.word(), name])],
        (Init::Openrc, Action::Enable) => vec![words(&["rc-update", "add", name, "default"])],
        (Init::Openrc, Action::Disable) => vec![words(&["rc-update", "del", name, "default"])],
        (Init::Openrc, action) => vec![words(&["rc-service", name, action.word()])],
        (Init::Scripts, Action::Enable | Action::Disable) => {
            return Err(
                "error: this init system has no enable or disable: every /etc/init.d/S* script \
                 runs at boot. Tell the user what would have to change instead."
                    .into(),
            );
        }
        (Init::Scripts, action) => vec![vec![script, action.word().to_owned()]],
    })
}

/// What running commands came to.
pub struct Ran {
    /// Each command, its exit code and output.
    pub text: String,
    /// Every command ran and exited with 0.
    pub ok: bool,
}

/// Runs `commands` in order and reports each one's exit code and output.
pub async fn run_commands(commands: &[Vec<String>], timeout: Duration, cap: usize) -> Ran {
    let mut out = String::new();
    let mut ok = true;
    for command in commands {
        let _ = writeln!(out, "$ {}", command.join(" "));
        let run = tokio::process::Command::new(&command[0])
            .args(&command[1..])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output();
        match tokio::time::timeout(timeout, run).await {
            Err(_) => {
                ok = false;
                let _ = writeln!(out, "timed out after {}s", timeout.as_secs());
            }
            Ok(Err(err)) => {
                ok = false;
                let _ = writeln!(out, "cannot run: {err}");
            }
            Ok(Ok(output)) => {
                ok &= output.status.success();
                let code = output
                    .status
                    .code()
                    .map_or_else(|| "killed by signal".into(), |c| c.to_string());
                let _ = writeln!(out, "exit code: {code}");
                for stream in [&output.stdout, &output.stderr] {
                    let text = String::from_utf8_lossy(stream);
                    let text = text.trim_end();
                    if !text.is_empty() {
                        out.push_str(&crate::tools::truncate(text.as_bytes(), cap));
                        out.push('\n');
                    }
                }
            }
        }
    }
    Ran { text: out, ok }
}

/// One way of checking that a step worked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// A host name resolves.
    Dns,
    /// `host:port` accepts a TCP connection.
    Tcp,
    /// A URL answers over HTTP(S).
    Http,
    /// A process of this name runs.
    Process,
    /// A service is running, as its init system says.
    Service,
}

impl Check {
    pub const NAMES: &[&str] = &["dns", "tcp", "http", "process", "service"];

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text.trim() {
            "dns" => Check::Dns,
            "tcp" => Check::Tcp,
            "http" => Check::Http,
            "process" => Check::Process,
            "service" => Check::Service,
            _ => return None,
        })
    }
}

/// Runs a check; the text starts with `ok:` or `failed:`.
pub async fn check(kind: Check, target: &str) -> String {
    let target = target.trim();
    let result = match kind {
        Check::Dns => tokio::time::timeout(CHECK_TIMEOUT, tokio::net::lookup_host((target, 0)))
            .await
            .map_err(|_| "timed out".to_owned())
            .and_then(|r| r.map_err(|e| e.to_string()))
            .map(|addresses| {
                let list: Vec<String> = addresses.map(|a| a.ip().to_string()).collect();
                format!("{target} resolves to {}", list.join(", "))
            }),
        Check::Tcp => tokio::time::timeout(CHECK_TIMEOUT, tokio::net::TcpStream::connect(target))
            .await
            .map_err(|_| "timed out".to_owned())
            .and_then(|r| r.map_err(|e| e.to_string()))
            .map(|stream| match stream.peer_addr() {
                Ok(peer) => format!("connected to {target} ({peer})"),
                Err(_) => format!("connected to {target}"),
            }),
        Check::Http => http(target).await,
        Check::Service => service_running(target).await,
        Check::Process => {
            let pids = Host::live().processes(target);
            if pids.is_empty() {
                Err(format!("no process named {target}"))
            } else {
                Ok(format!("{target} runs as pid {}", pids.join(", ")))
            }
        }
    };
    match result {
        Ok(text) => format!("ok: {text}"),
        Err(why) => format!("failed: {why}"),
    }
}

/// Whether the init system says `name` runs: its first status command
/// (`systemctl is-active`, `rc-service … status`, `/etc/init.d/… status`)
/// exits with 0.
async fn service_running(name: &str) -> Result<String, String> {
    if !valid_service(name) {
        return Err(format!("{name:?} is not a service name"));
    }
    let commands = service_commands(Host::live().init(), name, Action::Status)?;
    let ran = run_commands(&commands[..1], CHECK_TIMEOUT, 2048).await;
    if ran.ok {
        Ok(format!("{name} is running"))
    } else {
        Err(format!("{name} is not running\n{}", ran.text.trim_end()))
    }
}

async fn http(url: &str) -> Result<String, String> {
    let client = crate::net::builder()
        .timeout(CHECK_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let response = client.get(url).send().await.map_err(|e| format!("{e:#}"))?;
    Ok(format!("{url} answered {}", response.status()))
}

impl Host {
    /// Pids of the processes named `name`, from /proc/<pid>/comm.
    fn processes(&self, name: &str) -> Vec<String> {
        let mut pids: Vec<String> = std::fs::read_dir(self.path("proc"))
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|pid| pid.chars().all(|c| c.is_ascii_digit()))
            .filter(|pid| self.read(&format!("proc/{pid}/comm")).as_deref() == Some(name))
            .collect();
        pids.sort_by_key(|p| p.parse::<u64>().unwrap_or(0));
        pids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn inspect_reads_the_board_network_and_services_without_keys() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "proc/device-tree/model",
            "Raspberry Pi 5 Model B Rev 1.0\0",
        );
        write(
            root,
            "etc/os-release",
            "NAME=\"Buildroot\"\nPRETTY_NAME=\"Runo 1.0\"\n",
        );
        write(root, "etc/timezone", "Asia/Shanghai\n");
        write(root, "sys/class/net/eth0/operstate", "up\n");
        std::fs::create_dir_all(root.join("sys/class/net/eth0/device")).unwrap();
        write(root, "sys/class/net/wlan0/operstate", "down\n");
        std::fs::create_dir_all(root.join("sys/class/net/wlan0/wireless")).unwrap();
        std::fs::create_dir_all(root.join("sys/class/net/lo")).unwrap();
        write(
            root,
            "proc/net/route",
            "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\n\
             eth0\t0001A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\n\
             eth0\t00000000\t0101A8C0\t0003\t0\t0\t0\t00000000\n",
        );
        write(
            root,
            "proc/net/ipv6_route",
            "fe800000000000000000000000000000 40 00000000000000000000000000000000 00 00000000000000000000000000000000 00000100 00000001 00000000 00000001     eth0\n",
        );
        write(root, "etc/resolv.conf", "# x\nnameserver 192.168.1.1\n");
        write(
            root,
            "etc/wpa_supplicant/wpa_supplicant.conf",
            "network={\n  ssid=\"Home\"\n  psk=\"secret-pass\"\n}\n",
        );
        write(root, "etc/init.d/S40network", "");
        write(root, "etc/init.d/S49ntp", "");

        let host = Host::at(root);
        assert!(host.is_raspberry_pi());
        assert_eq!(host.init(), Init::Scripts);
        let text = host.inspect();
        for expected in [
            "device: Raspberry Pi 5 Model B Rev 1.0",
            "os: Runo 1.0",
            "timezone: Asia/Shanghai",
            "eth0 (ethernet, up)",
            "wlan0 (wifi, down)",
            "default route: IPv4 eth0, IPv6 none",
            "dns servers: 192.168.1.1",
            "network configured by: wpa_supplicant",
            "wpa_supplicant.conf: \"Home\"",
            "services in /etc/init.d (2): S40network S49ntp",
            "init: init scripts",
        ] {
            assert!(
                text.contains(expected),
                "{expected:?} missing from:\n{text}"
            );
        }
        assert!(!text.contains("secret-pass"), "{text}");
        assert!(!text.contains("lo ("), "{text}");
        assert!(!text.contains("not a Raspberry Pi"), "{text}");
    }

    #[test]
    fn inspect_explains_the_runo_system_image() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "etc/runo-system", "board=rpi5\nversion=1.0\n");
        write(
            root,
            "etc/wpa_supplicant.conf",
            "ctrl_interface=/run/wpa_supplicant\nnetwork={\n\tssid=\"Home\"\n\tpsk=0123abcd\n}\n",
        );
        let text = Host::at(root).inspect();
        for expected in [
            "runo system: this is Runo's own image",
            "network configured by: wpa_supplicant",
            "wifi networks in /etc/wpa_supplicant.conf: \"Home\"",
        ] {
            assert!(
                text.contains(expected),
                "{expected:?} missing from:\n{text}"
            );
        }
        assert!(!text.contains("0123abcd"), "{text}");

        let plain = tempfile::tempdir().unwrap();
        assert!(!Host::at(plain.path()).inspect().contains("runo system"));
    }

    #[test]
    fn other_boards_are_named_and_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let host = Host::at(dir.path());
        assert!(!host.is_raspberry_pi());
        assert!(host.inspect().contains("not a Raspberry Pi"));
        std::fs::create_dir_all(dir.path().join("run/systemd/system")).unwrap();
        assert_eq!(host.init(), Init::Systemd);
    }

    #[test]
    fn services_map_to_each_init_system_and_names_are_checked() {
        let cmds = |init, action| service_commands(init, "wpa_supplicant", action);
        assert_eq!(
            cmds(Init::Systemd, Action::Restart).unwrap(),
            vec![vec!["systemctl", "restart", "wpa_supplicant"]]
        );
        assert_eq!(
            cmds(Init::Openrc, Action::Enable).unwrap(),
            vec![vec!["rc-update", "add", "wpa_supplicant", "default"]]
        );
        assert_eq!(
            cmds(Init::Openrc, Action::Status).unwrap(),
            vec![vec!["rc-service", "wpa_supplicant", "status"]]
        );
        assert_eq!(
            cmds(Init::Scripts, Action::Start).unwrap(),
            vec![vec!["/etc/init.d/wpa_supplicant", "start"]]
        );
        assert!(cmds(Init::Scripts, Action::Enable).is_err());
        assert_eq!(cmds(Init::Systemd, Action::Status).unwrap().len(), 2);

        for good in [
            "S40network",
            "ntpd",
            "getty@tty1",
            "systemd-timesyncd.service",
        ] {
            assert!(valid_service(good), "{good}");
        }
        for bad in [
            "",
            "../sbin/reboot",
            "-rf",
            "a b",
            "x;reboot",
            &"a".repeat(65),
        ] {
            assert!(!valid_service(bad), "{bad}");
        }
        for name in Action::NAMES {
            assert_eq!(Action::parse(name).unwrap().word(), *name);
        }
        assert!(!Action::Status.changes() && Action::Stop.changes());
    }

    #[tokio::test]
    async fn checks_report_ok_or_failed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        assert!(check(Check::Tcp, &address).await.starts_with("ok: "));
        drop(listener);
        assert!(check(Check::Tcp, &address).await.starts_with("failed: "));
        assert!(check(Check::Dns, "localhost").await.starts_with("ok: "));
        assert!(
            check(Check::Process, "no-such-process-runo")
                .await
                .starts_with("failed: ")
        );
        for name in Check::NAMES {
            assert!(Check::parse(name).is_some());
        }

        let out = run_commands(
            &[vec!["sh".into(), "-c".into(), "echo hi; exit 3".into()]],
            Duration::from_secs(5),
            1024,
        )
        .await;
        assert!(!out.ok);
        let out = out.text;
        assert!(out.contains("exit code: 3") && out.contains("hi"), "{out}");
        assert!(check(Check::Service, "../x").await.starts_with("failed: "));
    }

    #[test]
    fn processes_are_found_by_name() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "proc/12/comm", "ntpd\n");
        write(dir.path(), "proc/7/comm", "ntpd\n");
        write(dir.path(), "proc/9/comm", "sh\n");
        write(dir.path(), "proc/self/comm", "ntpd\n");
        assert_eq!(Host::at(dir.path()).processes("ntpd"), vec!["7", "12"]);
    }
}
