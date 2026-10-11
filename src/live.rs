//! The settings in effect while the program runs. They are read at
//! startup, and read again when the `settings` tool changes them, so a
//! change made in chat applies at once without a restart. Before
//! `init` (tests, one-off commands) nothing is live and callers keep the
//! configuration they were built with.

use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use anyhow::Result;

use crate::config::Config;

struct Live {
    path: PathBuf,
    config: RwLock<Config>,
}

static LIVE: OnceLock<Live> = OnceLock::new();

/// Makes `config`, loaded from `path`, the live settings; only the first
/// call counts.
pub fn init(path: &Path, config: &Config) {
    let _ = LIVE.set(Live {
        path: path.to_owned(),
        config: RwLock::new(config.clone()),
    });
}

/// Reads from the live settings; `None` before `init`.
pub fn read<T>(f: impl FnOnce(&Config) -> T) -> Option<T> {
    let live = LIVE.get()?;
    let config = live.config.read().unwrap_or_else(|p| p.into_inner());
    Some(f(&config))
}

/// Loads the file again after it changed.
pub fn reload() -> Result<()> {
    let Some(live) = LIVE.get() else {
        return Ok(());
    };
    let config = Config::load(&live.path)?;
    *live.config.write().unwrap_or_else(|p| p.into_inner()) = config;
    Ok(())
}
