//! runo-flash: writes the Runo system image to an SD card from a PC
//! (Linux or Windows), with screens in the terminal.

mod disk;
mod image;
mod release;
mod text;
mod ui;
mod write;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use crate::text::t;

#[derive(Parser)]
#[command(
    name = "runo-flash",
    version,
    about = "Writes the Runo system image to an SD card"
)]
struct Args {
    /// The image: runo-<board>.img.xz, the .zip from GitHub Actions, or an .img
    /// (default: choose on the screen).
    image: Option<PathBuf>,
    /// The board: rpi5 or rpi4.
    #[arg(long, value_parser = ["rpi5", "rpi4"])]
    board: Option<String>,
    /// Language: en or zh (default: the system's).
    #[arg(long, value_parser = ["en", "zh"])]
    lang: Option<String>,
    /// Write to this file instead of a card, to try it out.
    #[arg(long, hide = true)]
    to_file: Option<PathBuf>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    text::set_chinese(match &args.lang {
        Some(lang) => lang == "zh",
        None => text::system_is_chinese(),
    });
    #[cfg(windows)]
    // SAFETY: no preconditions; Chinese needs UTF-8 on the old console.
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleOutputCP(65001);
    }
    if args.to_file.is_none() && disk::needs_admin() {
        return elevate(args.lang.is_none());
    }
    let options = ui::Options {
        image: args.image,
        board: args.board.as_deref().and_then(ui::Board::from_id),
        to_file: args.to_file,
    };
    match ui::run(options) {
        Ok(_) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("runo-flash: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// The arguments to start again with, keeping the language.
fn again_args(add_lang: bool) -> Vec<std::ffi::OsString> {
    let mut args: Vec<_> = std::env::args_os().skip(1).collect();
    if add_lang {
        args.push("--lang".into());
        args.push(if text::chinese() { "zh" } else { "en" }.into());
    }
    args
}

/// Starts again as root (sudo), which writing to a card needs.
#[cfg(unix)]
fn elevate(add_lang: bool) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let Ok(exe) = std::env::current_exe() else {
        return ExitCode::FAILURE;
    };
    let on_path = |name: &str| {
        std::env::var_os("PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
    };
    let Some(tool) = ["sudo", "doas"].into_iter().find(|t| on_path(t)) else {
        eprintln!(
            "{}",
            t(
                "Writing to a card needs root: run runo-flash as root.",
                "写 SD 卡需要 root 权限：请用 root 运行 runo-flash。"
            )
        );
        return ExitCode::FAILURE;
    };
    eprintln!(
        "{} ({tool})",
        t(
            "Writing to a card needs root; asking for it",
            "写 SD 卡需要 root 权限，正在申请"
        )
    );
    let err = std::process::Command::new(tool)
        .arg(exe)
        .args(again_args(add_lang))
        .exec();
    eprintln!("{tool}: {err}");
    ExitCode::FAILURE
}

/// Starts again as administrator, after Windows asks the person.
#[cfg(windows)]
fn elevate(add_lang: bool) -> ExitCode {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(Some(0)).collect()
    }
    let Ok(exe) = std::env::current_exe() else {
        return ExitCode::FAILURE;
    };
    let params = again_args(add_lang)
        .iter()
        .map(|a| format!("\"{}\"", a.to_string_lossy().replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(" ");
    let verb = wide("runas".as_ref());
    let file = wide(exe.as_os_str());
    let params = wide(params.as_ref());
    // SAFETY: the strings are NUL-terminated and live through the call.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // Above 32 means it started.
    if result as usize > 32 {
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "{}",
            t(
                "Writing to a card needs administrator rights: run runo-flash as administrator.",
                "写 SD 卡需要管理员权限：请右键“以管理员身份运行”。"
            )
        );
        ExitCode::FAILURE
    }
}

#[cfg(not(any(unix, windows)))]
fn elevate(_: bool) -> ExitCode {
    ExitCode::FAILURE
}
