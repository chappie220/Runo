//! Keyboards from `/dev/input/event*`: every device with letter keys,
//! grabbed so the kernel's console does not get the keys as well, found
//! again when one is plugged in.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;

use super::keys::{self, EV_KEY, EV_LED, EV_SYN, KEY_A, KEY_SPACE, KEY_Z, LED_CAPSL};

pub enum Message {
    /// (type, code, value) of events read.
    Events(Vec<(u16, u16, i32)>),
    /// The device went away.
    Gone(PathBuf),
}

/// `_IOC(dir, 'E', nr, size)`.
fn ioc(dir: u32, nr: u32, size: usize) -> u32 {
    (dir << 30) | ((size as u32) << 16) | ((b'E' as u32) << 8) | nr
}

const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

fn has_bit(bits: &[u8], bit: u16) -> bool {
    bits.get(bit as usize / 8)
        .is_some_and(|b| b & (1 << (bit % 8)) != 0)
}

/// Opens `path` if it is a keyboard, and grabs it.
fn open_keyboard(path: &Path) -> Option<OwnedFd> {
    let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let flags = libc::O_NONBLOCK | libc::O_CLOEXEC;
    // Writable for the CapsLock light, when allowed.
    // SAFETY: open(2) with a valid path.
    let fd = unsafe {
        match libc::open(name.as_ptr(), libc::O_RDWR | flags) {
            -1 => libc::open(name.as_ptr(), libc::O_RDONLY | flags),
            fd => fd,
        }
    };
    if fd < 0 {
        return None;
    }
    // SAFETY: just opened, owned here.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut types = [0u8; 4];
    let mut keys = [0u8; 96];
    // SAFETY: EVIOCGBIT fills at most the given length.
    let ok = unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            ioc(IOC_READ, 0x20, types.len()) as _,
            types.as_mut_ptr(),
        ) >= 0
            && libc::ioctl(
                fd.as_raw_fd(),
                ioc(IOC_READ, 0x20 + EV_KEY as u32, keys.len()) as _,
                keys.as_mut_ptr(),
            ) >= 0
    };
    let keyboard = ok
        && has_bit(&types, EV_KEY)
        && [KEY_A, KEY_Z, KEY_SPACE].iter().all(|&k| has_bit(&keys, k));
    if !keyboard {
        return None;
    }
    // SAFETY: EVIOCGRAB takes an int by value.
    let grabbed = unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            ioc(IOC_WRITE, 0x90, std::mem::size_of::<libc::c_int>()) as _,
            1 as libc::c_int,
        )
    };
    if grabbed < 0 {
        // Another program has it.
        return None;
    }
    Some(fd)
}

/// The keyboards in use; their events arrive on the channel.
pub struct Keyboards {
    open: HashMap<PathBuf, Arc<AsyncFd<OwnedFd>>>,
    sender: mpsc::UnboundedSender<Message>,
}

impl Keyboards {
    pub fn new(sender: mpsc::UnboundedSender<Message>) -> Keyboards {
        Keyboards {
            open: HashMap::new(),
            sender,
        }
    }

    /// Opens keyboards not yet in use; true when one was.
    pub fn scan(&mut self) -> bool {
        let Ok(entries) = std::fs::read_dir("/dev/input") else {
            return false;
        };
        let mut found = false;
        for entry in entries.flatten() {
            let path = entry.path();
            let is_event = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("event"));
            if !is_event || self.open.contains_key(&path) {
                continue;
            }
            let Some(fd) = open_keyboard(&path) else {
                continue;
            };
            let Ok(fd) = AsyncFd::new(fd) else {
                continue;
            };
            let fd = Arc::new(fd);
            self.open.insert(path.clone(), fd.clone());
            tokio::spawn(read(path, fd, self.sender.clone()));
            found = true;
        }
        found
    }

    pub fn gone(&mut self, path: &Path) {
        self.open.remove(path);
    }

    /// Lights CapsLock on every keyboard.
    pub fn caps_light(&self, on: bool) {
        let mut bytes = keys::event_bytes(EV_LED, LED_CAPSL, on as i32);
        bytes.extend(keys::event_bytes(EV_SYN, 0, 0));
        for fd in self.open.values() {
            // SAFETY: writes a valid buffer; a read-only device refuses it.
            unsafe { libc::write(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
        }
    }
}

async fn read(path: PathBuf, fd: Arc<AsyncFd<OwnedFd>>, sender: mpsc::UnboundedSender<Message>) {
    let mut buffer = vec![0u8; keys::EVENT_SIZE * 64];
    loop {
        let Ok(mut guard) = fd.readable().await else {
            break;
        };
        match guard.try_io(|fd| {
            // SAFETY: reads into a valid buffer from an open descriptor.
            let n = unsafe { libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
            if n < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        }) {
            Ok(Ok(0)) | Ok(Err(_)) => break,
            Ok(Ok(n)) => {
                let events = keys::parse_events(&buffer[..n]);
                if sender.send(Message::Events(events)).is_err() {
                    return;
                }
            }
            Err(_would_block) => continue,
        }
    }
    let _ = sender.send(Message::Gone(path));
}
