//! Doing what needs root from an ordinary account: the command goes through
//! sudo or doas, which ask for the password on the terminal themselves.
//! This program never sees the password.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::i18n::{Tr, tr};

/// Tried in order; sudo is configured on most systems that have both.
const TOOLS: &[&str] = &["sudo", "doas"];

const ASKING: Tr = tr(
    "This needs root; running it with {} (it may ask for your password):\n  {}",
    "这一步需要 root，用 {} 运行（可能会要求输入你的密码）：\n  {}",
);
const NO_TOOL: Tr = tr(
    "this needs root, and neither sudo nor doas is installed: run it as root ({})",
    "这一步需要 root，但没有安装 sudo 或 doas：请用 root 运行（{}）",
);
const FAILED: Tr = tr("{} failed ({})", "{} 失败（{}）");
const NO_PACKAGE_MANAGER: Tr = tr(
    "no known package manager (apk, apt-get, dnf, pacman, zypper); install {} by hand",
    "没有找到已知的包管理器（apk、apt-get、dnf、pacman、zypper）；请手动安装 {}",
);

#[cfg(test)]
pub const ALL: &[Tr] = &[ASKING, NO_TOOL, FAILED, NO_PACKAGE_MANAGER];

pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// Whether `name` is an executable on PATH.
pub fn on_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            std::fs::metadata(dir.join(name))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

/// sudo or doas, whichever is installed.
pub fn tool() -> Option<&'static str> {
    TOOLS.iter().copied().find(|t| on_path(t))
}

/// Whether a command can be run as root here: already root, or a tool to
/// ask with and a terminal to ask on.
pub fn can_ask() -> bool {
    is_root() || (tool().is_some() && std::fs::File::open("/dev/tty").is_ok())
}

/// Runs `program args` as root, through sudo or doas when this account is
/// not root. The command is shown first, so the person knows what the
/// password is for.
pub fn run(program: &str, args: &[&str]) -> Result<()> {
    let shown = std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    let status = if is_root() {
        Command::new(program).args(args).status()
    } else {
        let Some(tool) = tool() else {
            bail!(NO_TOOL.with(&[&shown]));
        };
        eprintln!("{}", ASKING.with(&[tool, &format!("{tool} {shown}")]));
        Command::new(tool).arg(program).args(args).status()
    }
    .with_context(|| format!("cannot run {program}"))?;
    if !status.success() {
        bail!(FAILED.with(&[&shown, &status.to_string()]));
    }
    Ok(())
}

/// Runs this program again as root with `args`, in the current language.
pub fn rerun(args: &[&str]) -> Result<()> {
    let binary = std::env::current_exe().context("cannot find this binary")?;
    let lang = match crate::i18n::current() {
        crate::i18n::Lang::En => "en",
        crate::i18n::Lang::Zh => "zh",
    };
    let mut all = vec!["--lang", lang];
    all.extend_from_slice(args);
    run(&binary.to_string_lossy(), &all)
}

/// The package manager command that installs `packages`, for the first
/// manager found by `exists`.
fn install_command(
    packages: &[&str],
    exists: impl Fn(&str) -> bool,
) -> Option<(&'static str, Vec<String>)> {
    let (manager, first): (&'static str, &[&str]) = [
        ("apk", &["add"][..]),
        ("apt-get", &["install", "-y"][..]),
        ("dnf", &["install", "-y"][..]),
        ("pacman", &["-S", "--noconfirm", "--needed"][..]),
        ("zypper", &["--non-interactive", "install"][..]),
    ]
    .into_iter()
    .find(|(m, _)| exists(m))?;
    let args = first
        .iter()
        .chain(packages)
        .map(|s| (*s).to_owned())
        .collect();
    Some((manager, args))
}

/// Installs system packages with the host's package manager, as root.
pub fn install_packages(packages: &[&str]) -> Result<()> {
    let Some((manager, args)) = install_command(packages, on_path) else {
        bail!(NO_PACKAGE_MANAGER.with(&[&packages.join(" ")]));
    };
    if manager == "apt-get" {
        // A fresh system has no package lists yet; one command, one password.
        let script = format!("apt-get update -q && apt-get {}", args.join(" "));
        return run("sh", &["-c", &script]);
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run(manager, &args)
}

/// Whether this account can create files in `dir`.
pub fn writable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: c is a valid NUL-terminated path.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_first_package_manager_found() {
        let (m, args) = install_command(&["chromium"], |m| m == "apk").unwrap();
        assert_eq!((m, args), ("apk", vec!["add".into(), "chromium".into()]));
        let (m, args) = install_command(&["chromium", "git"], |m| m != "apk").unwrap();
        assert_eq!(m, "apt-get");
        assert_eq!(args, ["install", "-y", "chromium", "git"]);
        assert!(install_command(&["git"], |_| false).is_none());
    }

    #[test]
    fn every_text_has_both_languages() {
        crate::i18n::assert_complete(ALL);
    }
}
