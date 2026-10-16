//! English or Chinese, chosen once at start.

use std::sync::atomic::{AtomicBool, Ordering};

static CHINESE: AtomicBool = AtomicBool::new(false);

pub fn set_chinese(on: bool) {
    CHINESE.store(on, Ordering::Relaxed);
}

pub fn chinese() -> bool {
    CHINESE.load(Ordering::Relaxed)
}

/// The text in the language in use.
pub fn t(en: &'static str, zh: &'static str) -> &'static str {
    if chinese() { zh } else { en }
}

/// Whether the system speaks Chinese.
pub fn system_is_chinese() -> bool {
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(value) = std::env::var(var)
            && !value.is_empty()
        {
            return value.starts_with("zh");
        }
    }
    #[cfg(windows)]
    {
        // SAFETY: no preconditions. The low ten bits are the primary language.
        let lang = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() };
        return lang & 0x3ff == 0x04;
    }
    #[allow(unreachable_code)]
    false
}

/// A byte count for people: 1.8 GB, 512 MB.
pub fn size(bytes: u64) -> String {
    const GB: f64 = 1e9;
    const MB: f64 = 1e6;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else {
        format!("{:.0} MB", b / MB)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sizes_read_like_on_the_card() {
        assert_eq!(super::size(31_914_983_424), "31.9 GB");
        assert_eq!(super::size(268_435_456), "268 MB");
    }
}
