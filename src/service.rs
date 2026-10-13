//! Running the Gateway in the background: `runo service` (also
//! `runo gateway`) installs, starts, stops and reports on it.
//!
//! Three ways, picked per host and account:
//!
//! - **System** (root, OpenRC): /etc/init.d/runo under
//!   supervise-daemon, started at boot, running as `--user`.
//! - **User** (not root, OpenRC 0.60+ with user services): the same script in
//!   `$XDG_CONFIG_HOME/rc/init.d`, driven by `rc-service --user`; it starts
//!   with the account's OpenRC session (pam_openrc).
//! - **Detached** (no OpenRC, or no user services): `serve` started in its own
//!   session with a pid file in the state directory. Nothing starts it at
//!   boot.
//!
//! A user who is not root can still ask about a system service, but only
//! root can start or stop it.

use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::i18n::{Tr, tr};

const NAME: &str = "runo";
const OPENRC_RUN: &str = "/sbin/openrc-run";
const INIT_SCRIPT: &str = "/etc/init.d/runo";
const CONF_FILE: &str = "/etc/conf.d/runo";
const LOG_FILE: &str = "/var/log/runo.log";
/// What `service install` made before 0.5, under the old name.
const LEGACY_NAME: &str = crate::rename::LEGACY_NAME;
pub const LEGACY_INIT_SCRIPT: &str = "/etc/init.d/openclaw-rs";
const LEGACY_CONF_FILE: &str = "/etc/conf.d/openclaw-rs";
/// Under the state directory, for user and detached runs.
const USER_LOG: &str = "gateway.log";
const PID_FILE: &str = "gateway.pid";
/// Secrets copied from the installing shell into the owner-only conf.d file.
const FORWARDED_ENV: &[&str] = &[
    "OPENROUTER_API_KEY",
    "RUNO_TOKEN",
    "QQ_APP_SECRET",
    "FEISHU_APP_SECRET",
    "ZULIP_API_KEY",
    "TELEGRAM_BOT_TOKEN",
    "DISCORD_BOT_TOKEN",
    "MAIL_PASSWORD",
    "TYPESAFE_API_KEY",
];

const STARTED: Tr = tr("Gateway started (pid {}).", "Gateway 已启动（pid {}）。");
const ALREADY_RUNNING: Tr = tr(
    "Gateway is already running (pid {}).",
    "Gateway 已经在运行（pid {}）。",
);
const STOPPED: Tr = tr("Gateway stopped.", "Gateway 已停止。");
const NOT_RUNNING: Tr = tr("Gateway is not running.", "Gateway 没有运行。");
const RUNNING: Tr = tr(
    "Gateway is running (pid {}).",
    "Gateway 正在运行（pid {}）。",
);
const UP: Tr = tr("Gateway is running.", "Gateway 正在运行。");
const DIED: Tr = tr(
    "Gateway exited right after starting; the end of {}:",
    "Gateway 启动后马上退出了；{} 的最后几行：",
);
const NO_AUTOSTART: Tr = tr(
    "Started without OpenRC: it will not come back after a reboot. For that, install the system service as root (sudo runo gateway install --user {}), or enable OpenRC user services.",
    "没有通过 OpenRC 启动：重启机器后不会自动运行。需要开机自启的话，用 root 安装系统服务（sudo runo gateway install --user {}），或启用 OpenRC 用户服务。",
);
const USER_INSTALLED: Tr = tr(
    "installed the user service {} (state in {}); it starts with your OpenRC session (pam_openrc)",
    "已安装用户服务 {}（状态数据在 {}）；它随你的 OpenRC 会话启动（pam_openrc）",
);
const LOGS: Tr = tr(
    "logs: {} · status: runo service status",
    "日志：{} · 状态：runo service status",
);
const SYSTEM_NEEDS_ROOT: Tr = tr(
    "the Gateway runs as the system service {}; `{}` needs root, and there is no sudo or doas and terminal to ask on: run `runo gateway {}` as root",
    "Gateway 是系统服务 {}；`{}` 需要 root，但没有 sudo/doas 或终端可以询问密码：请用 root 运行 `runo gateway {}`",
);
const OTHER_USER: Tr = tr(
    "only root can install the service for another account ({}), and there is no sudo or doas and terminal to ask on; without root it runs as you",
    "只有 root 能为其他账户（{}）安装服务，但没有 sudo/doas 或终端可以询问密码；不用 root 时以你自己的身份运行",
);
const NO_ROOT: Tr = tr(
    "! The system service was not installed ({}); running in the background instead.",
    "! 没有安装系统服务（{}）；改为在后台运行。",
);
const DETACHED_REMOVED: Tr = tr(
    "Nothing was installed for this account; state and {} were kept.",
    "这个账户没有安装任何服务；状态数据和 {} 已保留。",
);
const REPLACED: Tr = tr(
    "Replaced the service {} with {}.",
    "已把旧服务 {} 换成 {}。",
);
const KIND_SYSTEM: Tr = tr("system service (OpenRC)", "系统服务（OpenRC）");
const KIND_USER: Tr = tr("user service (OpenRC --user)", "用户服务（OpenRC --user）");
const KIND_DETACHED: Tr = tr(
    "background process (no OpenRC service)",
    "后台进程（没有 OpenRC 服务）",
);

#[cfg(test)]
const ALL: &[Tr] = &[
    STARTED,
    ALREADY_RUNNING,
    STOPPED,
    NOT_RUNNING,
    RUNNING,
    UP,
    DIED,
    NO_AUTOSTART,
    USER_INSTALLED,
    LOGS,
    SYSTEM_NEEDS_ROOT,
    OTHER_USER,
    NO_ROOT,
    DETACHED_REMOVED,
    REPLACED,
    KIND_SYSTEM,
    KIND_USER,
    KIND_DETACHED,
];

pub struct Account {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

pub fn lookup_account(name: &str) -> Result<Account> {
    let c_name = CString::new(name).context("account name contains NUL")?;
    // SAFETY: getpwnam returns a pointer into static storage that stays valid
    // until the next passwd call; every field is copied out before returning.
    unsafe {
        let pw = libc::getpwnam(c_name.as_ptr());
        if pw.is_null() {
            bail!(crate::cli_text::NO_ACCOUNT.with(&[&format!("{name:?}")]));
        }
        account_from(pw)
    }
}

fn account_by_uid(uid: u32) -> Option<Account> {
    // SAFETY: as in lookup_account.
    unsafe {
        let pw = libc::getpwuid(uid);
        if pw.is_null() {
            return None;
        }
        account_from(pw).ok()
    }
}

fn current_account() -> Result<Account> {
    // SAFETY: as in lookup_account; geteuid has no preconditions.
    unsafe {
        let pw = libc::getpwuid(libc::geteuid());
        if pw.is_null() {
            bail!("this account has no passwd entry");
        }
        account_from(pw)
    }
}

/// SAFETY: `pw` must be a valid passwd entry.
unsafe fn account_from(pw: *const libc::passwd) -> Result<Account> {
    unsafe {
        Ok(Account {
            name: CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned(),
            uid: (*pw).pw_uid,
            gid: (*pw).pw_gid,
            home: PathBuf::from(CStr::from_ptr((*pw).pw_dir).to_string_lossy().into_owned()),
        })
    }
}

fn is_root() -> bool {
    crate::elevate::is_root()
}

/// openrc-run evaluates these assignments, so values must be single-quoted shell words.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// The system init script: runs as `account`, needs the network.
pub fn render_init_script(binary: &Path, account: &Account) -> String {
    let user = shell_quote(&format!("{}:{}", account.uid, account.gid));
    format!(
        r#"#!/sbin/openrc-run
# Managed by `runo service install`; rerun it to rewrite this file.

description="Runo Gateway"
supervisor=supervise-daemon
command={command}
command_args="serve"
command_user={user}
directory={home}
output_log={LOG_FILE}
error_log={LOG_FILE}
respawn_delay=5
respawn_max=10
respawn_period=300
retry="TERM/30/KILL/5"

depend() {{
	need net
	use dns logger
	after firewall
}}

start_pre() {{
	checkpath -f -o {user} -m 0640 {LOG_FILE}
}}
"#,
        command = shell_quote(&binary.display().to_string()),
        home = shell_quote(&account.home.display().to_string()),
    )
}

/// The user init script: runs as whoever owns the OpenRC session. System
/// services such as `net` are not visible from a user runlevel.
pub fn render_user_init_script(binary: &Path, home: &Path, log: &Path) -> String {
    let log = shell_quote(&log.display().to_string());
    format!(
        r#"#!/sbin/openrc-run
# Managed by `runo service install`; rerun it to rewrite this file.

description="Runo Gateway"
supervisor=supervise-daemon
command={command}
command_args="serve"
directory={home}
output_log={log}
error_log={log}
respawn_delay=5
respawn_max=10
respawn_period=300
retry="TERM/30/KILL/5"

start_pre() {{
	checkpath -f -m 0600 {log}
}}
"#,
        command = shell_quote(&binary.display().to_string()),
        home = shell_quote(&home.display().to_string()),
    )
}

pub fn render_conf(home: &Path, state: &Path, env: &[(String, String)]) -> String {
    let mut lines = vec![
        "# Managed by `runo service install`. Owner-only: may contain secrets.".to_owned(),
        format!("export HOME={}", shell_quote(&home.display().to_string())),
    ];
    // Also under the old name, which releases before 0.5 read: going back to
    // one (scripts/update.sh --version) keeps the same state.
    for key in ["RUNO_HOME", "OPENCLAW_RS_HOME"] {
        lines.push(format!(
            "export {key}={}",
            shell_quote(&state.display().to_string())
        ));
    }
    for (key, value) in env {
        lines.push(format!("export {key}={}", shell_quote(value)));
    }
    lines.push(String::new());
    lines.join("\n")
}

fn forwarded_env() -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = FORWARDED_ENV
        .iter()
        .filter(|key| **key != "RUNO_TOKEN")
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| ((*key).to_owned(), v))
        })
        .collect();
    // The token under both names, as RUNO_HOME in render_conf.
    if let Some(token) = crate::config::env_var("TOKEN").and_then(|v| v.into_string().ok()) {
        env.push(("RUNO_TOKEN".into(), token.clone()));
        env.push(("OPENCLAW_RS_TOKEN".into(), token));
    }
    env
}

/// Where a user service lives: `$XDG_CONFIG_HOME/rc`, else `~/.config/rc`.
struct UserPaths {
    init: PathBuf,
    conf: PathBuf,
}

impl UserPaths {
    fn new(home: &Path) -> Self {
        Self::named(home, NAME)
    }

    fn legacy(home: &Path) -> Self {
        Self::named(home, LEGACY_NAME)
    }

    fn named(home: &Path, name: &str) -> Self {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let rc = config.join("rc");
        Self {
            init: rc.join("init.d").join(name),
            conf: rc.join("conf.d").join(name),
        }
    }
}

/// "0.62.6" → (0, 62); the first dotted number in `openrc --version`.
fn parse_openrc_version(text: &str) -> Option<(u32, u32)> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .find_map(|word| {
            let mut parts = word.split('.');
            let major = parts.next()?.parse().ok()?;
            let minor = parts.next()?.parse().ok()?;
            Some((major, minor))
        })
}

/// OpenRC with user services (0.60 and newer) and a session to run them in.
fn user_services_available() -> bool {
    if !Path::new(OPENRC_RUN).exists()
        || std::env::var_os("XDG_RUNTIME_DIR").is_none_or(|v| v.is_empty())
    {
        return false;
    }
    Command::new("openrc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| parse_openrc_version(&String::from_utf8_lossy(&o.stdout)))
        .is_some_and(|version| version >= (0, 60))
}

enum Backend {
    /// /etc/init.d; `root` says whether this process may control it.
    System {
        root: bool,
    },
    User(UserPaths),
    Detached,
}

impl Backend {
    fn detect() -> Result<Self> {
        let openrc = Path::new(OPENRC_RUN).exists();
        if is_root() {
            return Ok(if openrc {
                Self::System { root: true }
            } else {
                Self::Detached
            });
        }
        let account = current_account()?;
        let user = UserPaths::new(&account.home);
        // Whatever is installed wins, so start and stop find what install made.
        if openrc && user.init.exists() {
            return Ok(Self::User(user));
        }
        if openrc && Path::new(INIT_SCRIPT).exists() && detached_pid().is_none() {
            return Ok(Self::System { root: false });
        }
        if detached_pid().is_none() && user_services_available() {
            return Ok(Self::User(user));
        }
        Ok(Self::Detached)
    }

    fn rc_service(&self, action: &str) -> Result<()> {
        match self {
            Self::System { root: true } => run("rc-service", &[NAME, action]),
            Self::System { root: false } => {
                if !crate::elevate::can_ask() {
                    bail!(SYSTEM_NEEDS_ROOT.with(&[
                        INIT_SCRIPT,
                        &format!("service {action}"),
                        action
                    ]))
                }
                crate::elevate::run("rc-service", &[NAME, action])
            }
            Self::User(_) => run("rc-service", &["--user", NAME, action]),
            Self::Detached => unreachable!("detached runs do not use OpenRC"),
        }
    }

    /// `rc-service status` succeeds only when started. OpenRC's state under
    /// /run is root-only, so an ordinary account asking about the system
    /// service looks for the Gateway process instead.
    fn rc_running(&self) -> bool {
        if matches!(self, Self::System { root: false }) {
            return gateway_pids().next().is_some();
        }
        let mut cmd = Command::new("rc-service");
        if matches!(self, Self::User(_)) {
            cmd.arg("--user");
        }
        cmd.args([NAME, "status"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn kind(&self) -> Tr {
        match self {
            Self::System { .. } => KIND_SYSTEM,
            Self::User(_) => KIND_USER,
            Self::Detached => KIND_DETACHED,
        }
    }

    fn log(&self) -> String {
        match self {
            Self::System { .. } => LOG_FILE.to_owned(),
            _ => state_file(USER_LOG).display().to_string(),
        }
    }
}

fn state_file(name: &str) -> PathBuf {
    crate::config::state_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(name)
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("cannot run {program}"))?;
    if !status.success() {
        bail!("{program} {} failed ({status})", args.join(" "));
    }
    Ok(())
}

fn write_file(path: &Path, contents: &str, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

fn remove_files(paths: &[&Path]) -> Result<()> {
    for path in paths {
        if let Err(err) = std::fs::remove_file(path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            return Err(err).with_context(|| format!("cannot remove {}", path.display()));
        }
    }
    Ok(())
}

/// Installs the service and starts it. `user` (root only) is the account it
/// runs as; without root it is always this account.
pub fn install(user: Option<&str>, config: Option<&Path>) -> Result<()> {
    let binary = std::env::current_exe()?.canonicalize()?;
    let env = forwarded_env();
    replace_legacy(&binary)?;
    if is_root() {
        if !Path::new(OPENRC_RUN).exists() {
            bail!(crate::cli_text::SERVICE_NO_OPENRC.now());
        }
        let account = lookup_account(user.unwrap_or("root"))?;
        let state = account_state(&account);
        let key_in_config = crate::config::Config::load(&state.join("config.toml"))
            .is_ok_and(|c| c.model.api_key.is_some_and(|k| !k.trim().is_empty()));
        if !key_in_config && !env.iter().any(|(key, _)| key == "OPENROUTER_API_KEY") {
            eprintln!("{}", crate::cli_text::SERVICE_NO_KEY.with(&[CONF_FILE]));
        }
        write_file(
            Path::new(CONF_FILE),
            &render_conf(&account.home, &state, &env),
            0o600,
        )?;
        write_file(
            Path::new(INIT_SCRIPT),
            &render_init_script(&binary, &account),
            0o755,
        )?;
        run("rc-update", &["add", NAME, "default"])?;
        run("rc-service", &[NAME, "restart"])?;
        println!(
            "{}",
            crate::cli_text::SERVICE_INSTALLED.with(&[
                INIT_SCRIPT,
                &account.name,
                &account.home.display().to_string()
            ])
        );
        println!("{}", crate::cli_text::SERVICE_LOGS.with(&[LOG_FILE, NAME]));
        return Ok(());
    }

    let account = current_account()?;
    if let Some(user) = user
        && user != account.name
    {
        if !crate::elevate::can_ask() {
            bail!(OTHER_USER.with(&[user]));
        }
        return crate::elevate::rerun(&["gateway", "install", "--user", user]);
    }
    let state = crate::config::state_dir()?;
    let paths = UserPaths::new(&account.home);
    let openrc = Path::new(OPENRC_RUN).exists();
    let user_service = openrc && (paths.init.exists() || user_services_available());
    // Without user services, the system service is the one that survives a
    // reboot: ask for root rather than settle for a background process.
    if openrc && !user_service && crate::elevate::can_ask() {
        if detached_pid().is_some() {
            detached_stop()?;
        }
        match crate::elevate::rerun(&["gateway", "install", "--user", &account.name]) {
            Ok(()) => return Ok(()),
            Err(err) => eprintln!("{}", NO_ROOT.with(&[&format!("{err:#}")])),
        }
    }
    if user_service {
        let log = state.join(USER_LOG);
        std::fs::create_dir_all(&state)
            .with_context(|| format!("cannot create {}", state.display()))?;
        write_file(
            &paths.conf,
            &render_conf(&account.home, &state, &env),
            0o600,
        )?;
        write_file(
            &paths.init,
            &render_user_init_script(&binary, &account.home, &log),
            0o755,
        )?;
        run("rc-update", &["--user", "add", NAME, "default"])?;
        run("rc-service", &["--user", NAME, "restart"])?;
        println!(
            "{}",
            USER_INSTALLED.with(&[
                &paths.init.display().to_string(),
                &state.display().to_string()
            ])
        );
        println!("{}", LOGS.with(&[&log.display().to_string()]));
        return Ok(());
    }

    detached_start(&binary, config)?;
    println!("{}", NO_AUTOSTART.with(&[&account.name]));
    Ok(())
}

/// Stops and removes what `install` made; state and logs are kept.
pub fn uninstall() -> Result<()> {
    match Backend::detect()? {
        Backend::System { root: true } => {
            if !Path::new(INIT_SCRIPT).exists() {
                println!(
                    "{}",
                    crate::cli_text::SERVICE_NOT_INSTALLED.with(&[INIT_SCRIPT])
                );
                return Ok(());
            }
            // Stop and runlevel removal fail harmlessly when already done.
            let _ = run("rc-service", &[NAME, "stop"]);
            let _ = run("rc-update", &["del", NAME, "default"]);
            remove_files(&[Path::new(INIT_SCRIPT), Path::new(CONF_FILE)])?;
            println!(
                "{}",
                crate::cli_text::SERVICE_REMOVED.with(&[INIT_SCRIPT, CONF_FILE, LOG_FILE])
            );
        }
        Backend::System { root: false } => {
            if !crate::elevate::can_ask() {
                bail!(SYSTEM_NEEDS_ROOT.with(&[INIT_SCRIPT, "service uninstall", "uninstall"]))
            }
            crate::elevate::rerun(&["gateway", "uninstall"])?;
        }
        Backend::User(paths) => {
            if !paths.init.exists() {
                println!(
                    "{}",
                    crate::cli_text::SERVICE_NOT_INSTALLED
                        .with(&[&paths.init.display().to_string()])
                );
                return Ok(());
            }
            let _ = run("rc-service", &["--user", NAME, "stop"]);
            let _ = run("rc-update", &["--user", "del", NAME, "default"]);
            remove_files(&[&paths.init, &paths.conf])?;
            println!(
                "{}",
                crate::cli_text::SERVICE_REMOVED.with(&[
                    &paths.init.display().to_string(),
                    &paths.conf.display().to_string(),
                    &state_file(USER_LOG).display().to_string()
                ])
            );
        }
        Backend::Detached => {
            if detached_pid().is_some() {
                detached_stop()?;
            }
            println!(
                "{}",
                DETACHED_REMOVED.with(&[&state_file(USER_LOG).display().to_string()])
            );
        }
    }
    Ok(())
}

/// `config` is passed on to a detached `serve`; OpenRC services use the
/// account's default config.
pub fn start(config: Option<&Path>) -> Result<()> {
    match Backend::detect()? {
        Backend::Detached => detached_start(&std::env::current_exe()?.canonicalize()?, config),
        backend => backend.rc_service("start"),
    }
}

pub fn stop() -> Result<()> {
    match Backend::detect()? {
        Backend::Detached => detached_stop(),
        backend => backend.rc_service("stop"),
    }
}

pub fn restart(config: Option<&Path>) -> Result<()> {
    match Backend::detect()? {
        Backend::Detached => {
            if detached_pid().is_some() {
                detached_stop()?;
            }
            detached_start(&std::env::current_exe()?.canonicalize()?, config)
        }
        backend => backend.rc_service("restart"),
    }
}

/// Prints how the Gateway runs and whether it is running; `false` when not.
pub fn status() -> Result<bool> {
    let backend = Backend::detect()?;
    let running = match &backend {
        Backend::Detached => detached_pid(),
        Backend::System { root: false } => gateway_pids().next(),
        // OpenRC knows the pid; `rc-service status` only says whether it runs.
        _ => backend.rc_running().then_some(0),
    };
    println!("{}", backend.kind().now());
    match running {
        Some(0) => println!("{}", UP.now()),
        Some(pid) => println!("{}", RUNNING.with(&[&pid.to_string()])),
        None => println!("{}", NOT_RUNNING.now()),
    }
    println!("{}", LOGS.with(&[&backend.log()]));
    Ok(running.is_some())
}

/// Restarts a running Gateway this process may control; `true` when it did.
pub fn restart_if_running() -> bool {
    let Ok(backend) = Backend::detect() else {
        return false;
    };
    match backend {
        Backend::System { root: false } => false,
        Backend::Detached => {
            // Keeps the arguments it was started with.
            let Some(pid) = detached_pid() else {
                return false;
            };
            let config = detached_config(pid);
            detached_stop().is_ok()
                && std::env::current_exe()
                    .and_then(|b| b.canonicalize())
                    .is_ok_and(|b| detached_start(&b, config.as_deref()).is_ok())
        }
        backend => backend.rc_running() && backend.rc_service("restart").is_ok(),
    }
}

/// One line for `doctor`: how the Gateway runs, `None` when nothing is set up.
pub fn describe() -> Option<String> {
    let backend = Backend::detect().ok()?;
    let installed = match &backend {
        Backend::System { .. } => Path::new(INIT_SCRIPT).exists(),
        Backend::User(paths) => paths.init.exists(),
        Backend::Detached => detached_pid().is_some(),
    };
    if !installed {
        return None;
    }
    let running = match &backend {
        Backend::Detached => true,
        _ => backend.rc_running(),
    };
    let state = if running { UP.now() } else { NOT_RUNNING.now() };
    Some(format!(
        "{} · {} · {}",
        backend.kind().now(),
        state,
        backend.log()
    ))
}

/// The pid in the pid file, when that process is still a Gateway.
fn detached_pid() -> Option<u32> {
    let text = std::fs::read_to_string(state_file(PID_FILE)).ok()?;
    let pid: u32 = text.trim().parse().ok()?;
    is_gateway(pid).then_some(pid)
}

/// Every running Gateway but this process, found through /proc, which
/// any account may read.
fn gateway_pids() -> impl Iterator<Item = u32> {
    let me = std::process::id();
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(move |pid| *pid != me && is_gateway(*pid))
}

/// Whether `pid` is alive and runs `serve` (a reused pid is not ours).
fn is_gateway(pid: u32) -> bool {
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    args.iter().any(|a| *a == b"serve")
        && args.first().is_some_and(|a| {
            let program = String::from_utf8_lossy(a);
            program.contains(NAME) || program.contains(LEGACY_NAME)
        })
}

/// Whether a Gateway (under either name) is running, other than this process.
pub fn gateway_running() -> bool {
    gateway_pids().next().is_some()
}

/// Whether a service of this account (or the system one) still has the old name.
pub fn legacy_installed() -> bool {
    legacy_system_installed()
        || current_account().is_ok_and(|a| UserPaths::legacy(&a.home).init.exists())
}

/// Whether the system service is still installed under the old name.
pub fn legacy_system_installed() -> bool {
    Path::new(LEGACY_INIT_SCRIPT).exists()
}

/// Where `account`'s state goes: moved to the new name first unless a
/// Gateway is running, which may have it open.
fn account_state(account: &Account) -> PathBuf {
    if !gateway_running()
        && let Err(err) =
            crate::rename::move_state_dir(&account.home, Some((account.uid, account.gid)))
    {
        eprintln!("{err:#}");
    }
    if crate::rename::state_unmoved(&account.home) {
        account.home.join(crate::config::LEGACY_STATE_DIR_NAME)
    } else {
        account.home.join(crate::config::STATE_DIR_NAME)
    }
}

/// A value as `shell_quote` wrote it (or plain).
fn shell_unquote(value: &str) -> String {
    let value = value.trim();
    match value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
        Some(inner) => inner.replace(r"'\''", "'"),
        None => value.trim_matches('"').to_owned(),
    }
}

/// The state directory a conf.d file of a service from before 0.5 sets.
fn conf_state(conf: &str) -> Option<PathBuf> {
    conf.lines().find_map(|line| {
        let line = line.trim();
        let value = line
            .strip_prefix("export OPENCLAW_RS_HOME=")
            .or_else(|| line.strip_prefix("export RUNO_HOME="))?;
        Some(PathBuf::from(shell_unquote(value))).filter(|p| !p.as_os_str().is_empty())
    })
}

/// Where the service from before 0.5 of `account` keeps its state: a
/// directory its conf.d file chose stays where it is; the default one
/// moves to the new name.
fn legacy_service_state(conf: &str, account: &Account) -> PathBuf {
    let defaults = [
        account.home.join(crate::config::LEGACY_STATE_DIR_NAME),
        account.home.join(crate::config::STATE_DIR_NAME),
    ];
    match conf_state(conf) {
        Some(dir) if !defaults.contains(&dir) => dir,
        _ => account_state(account),
    }
}

/// The conf.d file of a service installed before 0.5, for the new one: the
/// same settings and secrets, the state directory at `state`, and each
/// `OPENCLAW_RS_` variable also under its new `RUNO_` name. The old names
/// stay, so an older release installed again still finds them.
fn carry_conf(old: &str, home: &Path, state: &Path) -> String {
    let mut conf = render_conf(home, state, &[]);
    for line in old.lines() {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with('#')
            || line.starts_with("export HOME=")
            || line.starts_with("export OPENCLAW_RS_HOME=")
            || line.starts_with("export RUNO_HOME=")
        {
            continue;
        }
        if let Some(rest) = line.strip_prefix("export OPENCLAW_RS_") {
            conf.push_str(&format!("export RUNO_{rest}\n"));
        }
        conf.push_str(line);
        conf.push('\n');
    }
    conf
}

/// Replaces a service installed under the old name with the same service
/// under the new one, running `binary`: stops it, moves its account's state
/// directory, carries its conf.d settings over and starts the new one.
/// `false` when this account has none to replace.
pub fn replace_legacy(binary: &Path) -> Result<bool> {
    if is_root() && Path::new(LEGACY_INIT_SCRIPT).exists() {
        let script = std::fs::read_to_string(LEGACY_INIT_SCRIPT).unwrap_or_default();
        let conf = std::fs::read_to_string(LEGACY_CONF_FILE).unwrap_or_default();
        let account = legacy_uid(&script)
            .and_then(account_by_uid)
            .map_or_else(|| lookup_account("root"), Ok)?;
        let _ = run("rc-service", &[LEGACY_NAME, "stop"]);
        let _ = run("rc-update", &["del", LEGACY_NAME, "default"]);
        let state = legacy_service_state(&conf, &account);
        write_file(
            Path::new(CONF_FILE),
            &carry_conf(&conf, &account.home, &state),
            0o600,
        )?;
        write_file(
            Path::new(INIT_SCRIPT),
            &render_init_script(binary, &account),
            0o755,
        )?;
        remove_files(&[Path::new(LEGACY_INIT_SCRIPT), Path::new(LEGACY_CONF_FILE)])?;
        run("rc-update", &["add", NAME, "default"])?;
        run("rc-service", &[NAME, "restart"])?;
        println!("{}", REPLACED.with(&[LEGACY_INIT_SCRIPT, INIT_SCRIPT]));
        return Ok(true);
    }
    if is_root() {
        return Ok(false);
    }
    let account = current_account()?;
    let legacy = UserPaths::legacy(&account.home);
    if legacy.init.exists() {
        let conf = std::fs::read_to_string(&legacy.conf).unwrap_or_default();
        let _ = run("rc-service", &["--user", LEGACY_NAME, "stop"]);
        let _ = run("rc-update", &["--user", "del", LEGACY_NAME, "default"]);
        let state = legacy_service_state(&conf, &account);
        let paths = UserPaths::new(&account.home);
        write_file(
            &paths.conf,
            &carry_conf(&conf, &account.home, &state),
            0o600,
        )?;
        write_file(
            &paths.init,
            &render_user_init_script(binary, &account.home, &state.join(USER_LOG)),
            0o755,
        )?;
        remove_files(&[&legacy.init, &legacy.conf])?;
        run("rc-update", &["--user", "add", NAME, "default"])?;
        run("rc-service", &["--user", NAME, "restart"])?;
        println!(
            "{}",
            REPLACED.with(&[
                &legacy.init.display().to_string(),
                &paths.init.display().to_string()
            ])
        );
        return Ok(true);
    }
    // A background Gateway an old binary started keeps the old directory
    // open: restart it around the move.
    if crate::config::env_var("HOME").is_none()
        && crate::rename::state_unmoved(&account.home)
        && let Some(pid) = detached_pid()
    {
        let config = detached_config(pid);
        detached_stop()?;
        crate::rename::move_state_dir(&account.home, None)?;
        detached_start(binary, config.as_deref())?;
        return Ok(true);
    }
    Ok(false)
}

/// The uid in an init script's `command_user='uid:gid'`.
fn legacy_uid(script: &str) -> Option<u32> {
    script.lines().find_map(|line| {
        let value = line.trim().strip_prefix("command_user=")?;
        value.trim_matches('\'').split(':').next()?.parse().ok()
    })
}

/// The `--config` a running detached Gateway was given.
fn detached_config(pid: u32) -> Option<PathBuf> {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let mut args = cmdline.split(|b| *b == 0);
    args.find(|a| *a == b"--config")?;
    use std::os::unix::ffi::OsStrExt;
    args.next()
        .map(|a| PathBuf::from(std::ffi::OsStr::from_bytes(a)))
}

fn detached_start(binary: &Path, config: Option<&Path>) -> Result<()> {
    use std::os::unix::process::CommandExt;
    if let Some(pid) = detached_pid() {
        println!("{}", ALREADY_RUNNING.with(&[&pid.to_string()]));
        return Ok(());
    }
    let state = crate::config::state_dir()?;
    std::fs::create_dir_all(&state)
        .with_context(|| format!("cannot create {}", state.display()))?;
    let log_path = state.join(USER_LOG);
    let log = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&log_path)
            .with_context(|| format!("cannot open {}", log_path.display()))?
    };
    let mut cmd = Command::new(binary);
    if let Some(config) = config {
        cmd.arg("--config").arg(std::path::absolute(config)?);
    }
    cmd.arg("serve")
        .env("RUNO_HOME", &state)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    if let Ok(home) = std::env::var("HOME") {
        cmd.current_dir(home);
    }
    // SAFETY: setsid is async-signal-safe; it detaches from the terminal so
    // closing it (SIGHUP) or Ctrl-C does not reach the Gateway.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("cannot start {}", binary.display()))?;
    let pid = child.id();
    write_file(&state.join(PID_FILE), &format!("{pid}\n"), 0o600)?;
    // A bad config or a busy port ends it at once; say so instead of "started".
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            let _ = std::fs::remove_file(state.join(PID_FILE));
            eprintln!("{}", DIED.with(&[&log_path.display().to_string()]));
            eprintln!("{}", tail(&log_path, 10));
            bail!("runo serve exited ({status})");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("{}", STARTED.with(&[&pid.to_string()]));
    println!("{}", LOGS.with(&[&log_path.display().to_string()]));
    Ok(())
}

fn detached_stop() -> Result<()> {
    let Some(pid) = detached_pid() else {
        let _ = std::fs::remove_file(state_file(PID_FILE));
        println!("{}", NOT_RUNNING.now());
        return Ok(());
    };
    let signal = |sig| {
        // SAFETY: kill has no memory preconditions; pid was checked to be ours.
        unsafe { libc::kill(pid as libc::pid_t, sig) }
    };
    signal(libc::SIGTERM);
    // As the OpenRC script: TERM, 30 s, then KILL.
    let deadline = Instant::now() + Duration::from_secs(30);
    while is_gateway(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    if is_gateway(pid) {
        signal(libc::SIGKILL);
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = std::fs::remove_file(state_file(PID_FILE));
    println!("{}", STOPPED.now());
    Ok(())
}

fn tail(path: &Path, lines: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pi() -> Account {
        Account {
            name: "pi".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/pi".into(),
        }
    }

    #[test]
    fn renders_quoted_service_files() {
        let account = pi();
        let script = render_init_script(Path::new("/usr/local/bin/runo"), &account);
        assert!(script.contains("command='/usr/local/bin/runo'"));
        assert!(script.contains("command_user='1000:1000'"));
        assert!(script.contains("need net"));
        let conf = render_conf(
            &account.home,
            &account.home.join(".runo"),
            &[("OPENROUTER_API_KEY".into(), "sk-'x".into())],
        );
        assert!(conf.contains("export RUNO_HOME='/home/pi/.runo'"));
        assert!(conf.contains("export OPENCLAW_RS_HOME='/home/pi/.runo'"));
        assert!(conf.contains(r"export OPENROUTER_API_KEY='sk-'\''x'"));
    }

    #[test]
    fn user_script_has_no_account_switch_or_system_dependencies() {
        let script = render_user_init_script(
            Path::new("/home/pi/.local/bin/runo"),
            Path::new("/home/pi"),
            Path::new("/home/pi/.runo/gateway.log"),
        );
        assert!(script.contains("command='/home/pi/.local/bin/runo'"));
        assert!(script.contains("output_log='/home/pi/.runo/gateway.log'"));
        assert!(!script.contains("command_user"));
        assert!(!script.contains("need net"));
    }

    #[test]
    fn carries_an_old_service_over() {
        let script = render_init_script(Path::new("/usr/local/bin/openclaw-rs"), &pi());
        assert_eq!(legacy_uid(&script), Some(1000));
        let old = "# Managed by `openclaw-rs service install`. Owner-only: may contain secrets.\n\
                   export HOME='/home/pi'\n\
                   export OPENCLAW_RS_HOME='/home/pi/.openclaw-rs'\n\
                   export OPENROUTER_API_KEY='sk-x'\n\
                   export OPENCLAW_RS_TOKEN='t'\n";
        let conf = carry_conf(old, Path::new("/home/pi"), Path::new("/home/pi/.runo"));
        assert!(conf.contains("export RUNO_HOME='/home/pi/.runo'"), "{conf}");
        assert!(
            conf.contains("export OPENCLAW_RS_HOME='/home/pi/.runo'"),
            "{conf}"
        );
        assert!(conf.contains("export OPENROUTER_API_KEY='sk-x'"), "{conf}");
        // Under both names, so an older release installed again still has it.
        assert!(conf.contains("export RUNO_TOKEN='t'"), "{conf}");
        assert!(conf.contains("export OPENCLAW_RS_TOKEN='t'"), "{conf}");
        assert_eq!(conf.matches("export HOME=").count(), 1, "{conf}");
        assert_eq!(conf.matches("_HOME=").count(), 2, "{conf}");

        // The default directory moves; one the conf.d file chose stays.
        assert_eq!(
            conf_state(old),
            Some(PathBuf::from("/home/pi/.openclaw-rs"))
        );
        let custom = "export OPENCLAW_RS_HOME='/data/it'\\''s'\n";
        assert_eq!(conf_state(custom), Some(PathBuf::from("/data/it's")));
        assert_eq!(
            legacy_service_state(custom, &pi()),
            PathBuf::from("/data/it's")
        );
        assert_eq!(conf_state("export HOME='/home/pi'\n"), None);
    }

    #[test]
    fn reads_the_openrc_version() {
        assert_eq!(
            parse_openrc_version("openrc (OpenRC) 0.62.6 (Gentoo Linux)"),
            Some((0, 62))
        );
        assert_eq!(parse_openrc_version("openrc (OpenRC) 0.52"), Some((0, 52)));
        assert_eq!(parse_openrc_version("nothing here"), None);
        assert!((0, 62) >= (0, 60) && (0, 52) < (0, 60));
    }

    #[test]
    fn this_process_is_not_a_gateway() {
        assert!(!is_gateway(std::process::id()));
        assert!(!is_gateway(u32::MAX));
    }

    #[test]
    fn looks_up_root() {
        let root = lookup_account("root").unwrap();
        assert_eq!(root.uid, 0);
        assert!(lookup_account("no-such-user-xyz").is_err());
    }

    #[test]
    fn every_text_has_both_languages() {
        crate::i18n::assert_complete(ALL);
    }
}
