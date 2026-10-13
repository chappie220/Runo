//! Moving an install made under the old name (openclaw-rs, before 0.5) to
//! runo: the state directory, the binary and the OpenRC service.
//!
//! - The state directory moves on its own: any command finding
//!   `~/.openclaw-rs` and no `~/.runo` renames it, unless a Gateway is
//!   running (it may have the databases open). Until then that Gateway and
//!   every command keep using the old directory.
//! - The binary and the service move with `runo update` or `runo migrate`.
//!   An openclaw-rs that updates itself puts the new binary under the old
//!   name and keeps the old service, so the first `update` after that (or
//!   install.sh) finishes the move.
//!
//! The old names stay behind as links to the new ones, so scripts, shell
//! history and an `update --rollback` to the old binary keep working.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::{LEGACY_STATE_DIR_NAME, STATE_DIR_NAME};
use crate::i18n::{Tr, tr};

pub const NAME: &str = "runo";
pub const LEGACY_NAME: &str = "openclaw-rs";

const MOVED_STATE: Tr = tr(
    "Moved {} to {}; the old name now points to it.",
    "已把 {} 移到 {}；旧名字现在指向新位置。",
);
const STATE_IN_USE: Tr = tr(
    "Still using {}: a Gateway is running from it. Run `runo migrate` to move it and the service to the new name.",
    "仍在使用 {}：有 Gateway 正在用它运行。运行 `runo migrate` 把它和服务一起迁移到新名字。",
);
const RENAMED_BINARY: Tr = tr(
    "Renamed {} to {}; the old name now points to it.",
    "已把 {} 改名为 {}；旧名字现在指向它。",
);
const NOTHING_TO_MOVE: Tr = tr(
    "Nothing left to move from openclaw-rs.",
    "没有需要从 openclaw-rs 迁移的内容了。",
);
pub const SERVICE_UNMOVED: Tr = tr(
    "The service still has its old name (openclaw-rs): run `runo migrate` to move it to runo.",
    "服务还在用旧名字（openclaw-rs）：运行 `runo migrate` 把它迁移到 runo。",
);
const NEEDS_ROOT: Tr = tr(
    "{} needs root to move, and there is no sudo or doas and terminal to ask on: run `runo migrate` as root",
    "迁移 {} 需要 root，但没有 sudo/doas 或终端可以询问密码：请用 root 运行 `runo migrate`",
);

#[cfg(test)]
pub const ALL: &[Tr] = &[
    MOVED_STATE,
    STATE_IN_USE,
    RENAMED_BINARY,
    NOTHING_TO_MOVE,
    SERVICE_UNMOVED,
    NEEDS_ROOT,
];

fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

fn is_real_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Whether `home` still has its state under the old name only.
pub fn state_unmoved(home: &Path) -> bool {
    is_real_dir(&home.join(LEGACY_STATE_DIR_NAME)) && !exists(&home.join(STATE_DIR_NAME))
}

/// Renames `home/.openclaw-rs` to `home/.runo` and leaves a link under the
/// old name, owned by `owner` when given. The paths when it moved.
pub fn move_state_dir(
    home: &Path,
    owner: Option<(u32, u32)>,
) -> Result<Option<(PathBuf, PathBuf)>> {
    if !state_unmoved(home) {
        return Ok(None);
    }
    let old = home.join(LEGACY_STATE_DIR_NAME);
    let new = home.join(STATE_DIR_NAME);
    std::fs::rename(&old, &new)
        .with_context(|| format!("cannot move {} to {}", old.display(), new.display()))?;
    std::os::unix::fs::symlink(STATE_DIR_NAME, &old)
        .with_context(|| format!("cannot link {} to {}", old.display(), new.display()))?;
    if let Some((uid, gid)) = owner {
        let _ = std::os::unix::fs::lchown(&old, Some(uid), Some(gid));
    }
    Ok(Some((old, new)))
}

/// At startup: moves this account's state directory when nothing chose
/// another one and no Gateway is running. What to tell the user, if anything.
pub fn move_own_state_dir() -> Option<String> {
    if crate::config::env_var("HOME").is_some() {
        return None;
    }
    let home = PathBuf::from(std::env::var_os("HOME").filter(|v| !v.is_empty())?);
    if !state_unmoved(&home) {
        return None;
    }
    if crate::service::gateway_running() {
        let old = home.join(LEGACY_STATE_DIR_NAME);
        return Some(STATE_IN_USE.with(&[&old.display().to_string()]));
    }
    match move_state_dir(&home, None) {
        Ok(Some((old, new))) => {
            Some(MOVED_STATE.with(&[&old.display().to_string(), &new.display().to_string()]))
        }
        Ok(None) => None,
        Err(err) => Some(format!("{err:#}")),
    }
}

/// The binary's path under the new name once it has moved: `binary` itself,
/// or `runo` beside it when `binary` is the old name.
pub fn current_binary(binary: &Path) -> PathBuf {
    let new = binary.with_file_name(NAME);
    if binary.file_name().is_some_and(|n| n == LEGACY_NAME) && is_real_file(&new) {
        new
    } else {
        binary.to_owned()
    }
}

/// What a binary under the old name in `dir` has to become, if anything.
fn binary_unmoved(dir: &Path) -> bool {
    is_real_file(&dir.join(LEGACY_NAME))
}

/// Puts the binary under the new name and the old name as a link to it.
/// Handles both ways an old install meets a new binary: an openclaw-rs that
/// replaced itself with this release, and a runo installed beside an old
/// openclaw-rs (which then becomes `runo.old`, for `update --rollback`).
/// Returns where the binary now is.
pub fn move_binary(binary: &Path) -> Result<PathBuf> {
    let dir = binary.parent().context("the binary has no directory")?;
    let legacy = dir.join(LEGACY_NAME);
    let new = dir.join(NAME);
    let legacy_old = crate::update::old_path(&legacy);
    let new_old = crate::update::old_path(&new);
    if binary.file_name().is_some_and(|n| n == LEGACY_NAME) && is_real_file(binary) {
        std::fs::rename(binary, &new)
            .with_context(|| format!("cannot rename {} to {}", binary.display(), new.display()))?;
        if is_real_file(&legacy_old) && !exists(&new_old) {
            std::fs::rename(&legacy_old, &new_old)?;
        }
    } else if binary.file_name().is_some_and(|n| n == NAME) && is_real_file(&legacy) {
        if exists(&new_old) {
            std::fs::remove_file(&legacy)
        } else {
            std::fs::rename(&legacy, &new_old)
        }
        .with_context(|| format!("cannot replace {}", legacy.display()))?;
        if is_real_file(&legacy_old) {
            let _ = std::fs::remove_file(&legacy_old);
        }
    } else {
        return Ok(binary.to_owned());
    }
    std::os::unix::fs::symlink(NAME, &legacy)
        .with_context(|| format!("cannot link {} to {}", legacy.display(), new.display()))?;
    println!(
        "{}",
        RENAMED_BINARY.with(&[&legacy.display().to_string(), &new.display().to_string()])
    );
    Ok(new)
}

/// `runo migrate`, and the start of `runo update`: moves the binary, the
/// service and the state directory to the new name, asking for root when
/// the binary's directory or the service needs it. Returns where the binary
/// now is.
pub fn finish(quiet: bool) -> Result<PathBuf> {
    let binary = std::env::current_exe().context("cannot find this binary")?;
    let dir = binary.parent().context("the binary has no directory")?;
    let binary_left = binary.file_name().is_some_and(|n| n == LEGACY_NAME) || binary_unmoved(dir);
    let needs_root = !crate::elevate::is_root()
        && ((binary_left && !crate::elevate::writable(dir))
            || crate::service::legacy_system_installed());
    if needs_root {
        let what = if binary_left {
            dir.join(LEGACY_NAME).display().to_string()
        } else {
            crate::service::LEGACY_INIT_SCRIPT.to_owned()
        };
        if !crate::elevate::can_ask() {
            anyhow::bail!(NEEDS_ROOT.with(&[&what]));
        }
        crate::elevate::rerun(&["migrate"])?;
        return Ok(current_binary(&binary));
    }
    let mut moved = false;
    let binary = if binary_left {
        moved = true;
        move_binary(&binary)?
    } else {
        binary
    };
    moved |= crate::service::replace_legacy(&binary)?;
    if let Some(note) = move_own_state_dir() {
        moved = true;
        println!("{note}");
    }
    if !moved && !quiet {
        println!("{}", NOTHING_TO_MOVE.now());
    }
    Ok(binary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_have_both_languages() {
        crate::i18n::assert_complete(ALL);
    }

    #[test]
    fn moves_the_state_directory_once() {
        let home = tempfile::tempdir().unwrap();
        let old = home.path().join(LEGACY_STATE_DIR_NAME);
        std::fs::create_dir(&old).unwrap();
        std::fs::write(old.join("config.toml"), "x = 1\n").unwrap();
        let (from, to) = move_state_dir(home.path(), None).unwrap().unwrap();
        assert_eq!(from, old);
        assert_eq!(to, home.path().join(STATE_DIR_NAME));
        assert_eq!(
            std::fs::read_to_string(to.join("config.toml")).unwrap(),
            "x = 1\n"
        );
        // The old name reads through to the new directory.
        assert_eq!(
            std::fs::read_to_string(old.join("config.toml")).unwrap(),
            "x = 1\n"
        );
        assert!(std::fs::symlink_metadata(&old).unwrap().is_symlink());
        assert!(move_state_dir(home.path(), None).unwrap().is_none());
    }

    #[test]
    fn leaves_both_directories_alone_when_the_new_one_exists() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(LEGACY_STATE_DIR_NAME)).unwrap();
        std::fs::create_dir(home.path().join(STATE_DIR_NAME)).unwrap();
        assert!(move_state_dir(home.path(), None).unwrap().is_none());
        assert!(is_real_dir(&home.path().join(LEGACY_STATE_DIR_NAME)));
    }

    #[test]
    fn renames_a_binary_that_updated_itself() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join(LEGACY_NAME);
        std::fs::write(&legacy, "new").unwrap();
        std::fs::write(dir.path().join("openclaw-rs.old"), "old").unwrap();
        let now = move_binary(&legacy).unwrap();
        assert_eq!(now, dir.path().join(NAME));
        assert_eq!(std::fs::read_to_string(&now).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), "new");
        assert!(std::fs::symlink_metadata(&legacy).unwrap().is_symlink());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("runo.old")).unwrap(),
            "old"
        );
        assert_eq!(current_binary(&legacy), now);
    }

    #[test]
    fn keeps_an_old_binary_beside_a_new_install_for_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let new = dir.path().join(NAME);
        std::fs::write(&new, "new").unwrap();
        std::fs::write(dir.path().join(LEGACY_NAME), "old").unwrap();
        assert_eq!(move_binary(&new).unwrap(), new);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("runo.old")).unwrap(),
            "old"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(LEGACY_NAME)).unwrap(),
            "new"
        );
        // Nothing to do the second time.
        assert_eq!(move_binary(&new).unwrap(), new);
    }

    #[test]
    fn leaves_other_binaries_alone() {
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("runo-test");
        std::fs::write(&other, "x").unwrap();
        assert_eq!(move_binary(&other).unwrap(), other);
        assert!(!exists(&dir.path().join(LEGACY_NAME)));
    }
}
