//! `runo daemon install` / `uninstall`: the daemon as a system service
//! started at boot, under systemd, OpenRC or a plain /etc/init.d script
//! (BusyBox). It runs as root, with the state directory of whoever
//! installed it.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::i18n::{Tr, tr};
use crate::system::{Host, Init};

const UNIT: &str = "/etc/systemd/system/runod.service";
const SCRIPT: &str = "/etc/init.d/runod";
pub const LOG: &str = "/var/log/runod.log";

const NEEDS_ROOT: Tr = tr(
    "Installing the daemon needs root: sudo runo daemon install",
    "安装守护进程需要 root：sudo runo daemon install",
);
const INSTALLED: Tr = tr(
    "Installed and started the daemon ({}); it starts at boot. State: {}",
    "已安装并启动守护进程（{}）；开机自动启动。状态目录：{}",
);
const UNINSTALLED: Tr = tr(
    "Stopped and removed the daemon; its identity and trusted clients were kept in {}.",
    "已停止并移除守护进程；设备身份和可信客户端保留在 {}。",
);
const NOT_INSTALLED: Tr = tr("The daemon is not installed.", "守护进程没有安装。");

const BUILT_IN: Tr = tr(
    "On the Runo system the daemon is part of the system and starts at boot (/etc/init.d/runod).",
    "Runo 系统自带守护进程，开机即启动（/etc/init.d/runod）。",
);

#[cfg(test)]
pub const ALL: &[Tr] = &[NEEDS_ROOT, INSTALLED, UNINSTALLED, NOT_INSTALLED, BUILT_IN];

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

fn args(config: Option<&Path>) -> String {
    match config {
        Some(config) => format!(
            "--config {} daemon run",
            quote(&config.display().to_string())
        ),
        None => "daemon run".to_owned(),
    }
}

pub fn render_unit(binary: &Path, state: &Path, config: Option<&Path>) -> String {
    format!(
        "[Unit]\n\
         Description=Runo management daemon (runod)\n\
         After=network.target\n\
         \n\
         [Service]\n\
         Environment=RUNO_HOME={state}\n\
         ExecStart={binary} {args}\n\
         Restart=always\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        state = state.display(),
        binary = binary.display(),
        args = args(config),
    )
}

pub fn render_openrc(binary: &Path, state: &Path, config: Option<&Path>) -> String {
    format!(
        "#!/sbin/openrc-run\n\
         description=\"Runo management daemon (runod)\"\n\
         supervisor=supervise-daemon\n\
         command={binary}\n\
         command_args=\"{args}\"\n\
         output_log={LOG}\n\
         error_log={LOG}\n\
         export RUNO_HOME={state}\n\
         \n\
         depend() {{\n\
         \tneed net\n\
         }}\n",
        binary = quote(&binary.display().to_string()),
        args = args(config).replace('"', "\\\""),
        state = quote(&state.display().to_string()),
    )
}

pub fn render_script(binary: &Path, state: &Path, config: Option<&Path>) -> String {
    format!(
        "#!/bin/sh\n\
         # Runo management daemon (runod)\n\
         PIDFILE=/var/run/runod.pid\n\
         export RUNO_HOME={state}\n\
         case \"$1\" in\n\
         start)\n\
         \tstart-stop-daemon -S -b -m -p \"$PIDFILE\" -x /bin/sh -- -c \"exec {binary} {args} >>{LOG} 2>&1\"\n\
         \t;;\n\
         stop)\n\
         \tstart-stop-daemon -K -p \"$PIDFILE\" && rm -f \"$PIDFILE\"\n\
         \t;;\n\
         restart)\n\
         \t\"$0\" stop; sleep 1; \"$0\" start\n\
         \t;;\n\
         status)\n\
         \t[ -f \"$PIDFILE\" ] && kill -0 \"$(cat \"$PIDFILE\")\" 2>/dev/null\n\
         \t;;\n\
         *)\n\
         \techo \"usage: $0 start|stop|restart|status\"; exit 1\n\
         \t;;\n\
         esac\n",
        state = quote(&state.display().to_string()),
        binary = quote(&binary.display().to_string()),
        args = args(config),
    )
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("running {program}"))?;
    if !status.success() {
        bail!("{program} {} failed ({status})", args.join(" "));
    }
    Ok(())
}

fn write(path: &str, text: &str, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).with_context(|| format!("writing {path}"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

pub fn install(state: &Path, config: Option<&Path>) -> Result<()> {
    if crate::config::runo_system() {
        println!("{}", BUILT_IN.now());
        return Ok(());
    }
    if !is_root() {
        bail!("{}", NEEDS_ROOT.now());
    }
    let binary = std::env::current_exe().context("finding this program")?;
    let config: Option<PathBuf> = config.map(std::path::absolute).transpose()?;
    let config = config.as_deref();
    let init = Host::live().init();
    let how = match init {
        Init::Systemd => {
            write(UNIT, &render_unit(&binary, state, config), 0o644)?;
            run("systemctl", &["daemon-reload"])?;
            run("systemctl", &["enable", "--now", "runod"])?;
            run("systemctl", &["restart", "runod"])?;
            "systemd"
        }
        Init::Openrc => {
            write(SCRIPT, &render_openrc(&binary, state, config), 0o755)?;
            run("rc-update", &["add", "runod", "default"])?;
            run("rc-service", &["runod", "restart"])?;
            "OpenRC"
        }
        Init::Scripts => {
            write(SCRIPT, &render_script(&binary, state, config), 0o755)?;
            let _ = run(SCRIPT, &["stop"]);
            run(SCRIPT, &["start"])?;
            "/etc/init.d"
        }
    };
    println!("{}", INSTALLED.with(&[how, &state.display().to_string()]));
    Ok(())
}

pub fn uninstall(state: &Path) -> Result<()> {
    if crate::config::runo_system() {
        println!("{}", BUILT_IN.now());
        return Ok(());
    }
    if !is_root() {
        bail!("{}", NEEDS_ROOT.now());
    }
    let mut removed = false;
    if Path::new(UNIT).exists() {
        let _ = run("systemctl", &["disable", "--now", "runod"]);
        std::fs::remove_file(UNIT)?;
        let _ = run("systemctl", &["daemon-reload"]);
        removed = true;
    }
    if Path::new(SCRIPT).exists() {
        if Host::live().init() == Init::Openrc {
            let _ = run("rc-service", &["runod", "stop"]);
            let _ = run("rc-update", &["del", "runod", "default"]);
        } else {
            let _ = run(SCRIPT, &["stop"]);
        }
        std::fs::remove_file(SCRIPT)?;
        removed = true;
    }
    if removed {
        println!(
            "{}",
            UNINSTALLED.with(&[&super::dir(state).display().to_string()])
        );
    } else {
        println!("{}", NOT_INSTALLED.now());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_text_is_translated() {
        crate::i18n::assert_complete(ALL);
    }

    #[test]
    fn service_files_run_the_daemon_with_the_installers_state() {
        let binary = Path::new("/usr/bin/runo");
        let state = Path::new("/root/.runo");
        let unit = render_unit(binary, state, None);
        assert!(unit.contains("ExecStart=/usr/bin/runo daemon run\n"));
        assert!(unit.contains("Environment=RUNO_HOME=/root/.runo\n"));
        let openrc = render_openrc(binary, state, Some(Path::new("/etc/runo it's.toml")));
        assert!(openrc.contains(r#"command_args="--config '/etc/runo it'\''s.toml' daemon run""#));
        assert!(openrc.contains("supervisor=supervise-daemon"));
        let script = render_script(binary, state, None);
        assert!(script.contains("start-stop-daemon -S -b -m"));
        assert!(script.contains("exec '/usr/bin/runo' daemon run >>/var/log/runod.log 2>&1"));
    }
}
