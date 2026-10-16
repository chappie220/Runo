//! Writing the image to the card and reading it back.
//!
//! The first sector, with the partition table, is written last: until the
//! write is complete the card shows no partitions, so the computer does not
//! mount any of them halfway (Windows refuses writes to a mounted volume).
//! With settings to add (Wi-Fi, the device password), the boot partition is
//! kept in memory until it has all arrived, and `runo-wifi.txt` and
//! `runo-password.txt` are added to it before it is written.

use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};

use anyhow::{Result, bail};
use sha2::{Digest, Sha256};

use crate::text::{size, t};

pub const SECTOR: usize = 512;
const CHUNK: usize = 4 << 20;
/// How much is written before waiting for the card to take it. Without
/// it the computer's memory takes gigabytes at once, the progress runs
/// ahead of the card, and the end is a long wait with nothing moving.
const SYNC_EVERY: u64 = 64 << 20;
/// The most of the image kept in memory to add the Wi-Fi settings.
const MAX_HEAD: u64 = 1 << 30;

/// Where the image goes: a card, or a file in tests.
pub trait Device: Read + Write + Seek + Send {
    /// Makes sure everything written is on the card.
    fn sync(&mut self) -> io::Result<()>;
    /// Forgets what the computer remembers of the card, so reading it back
    /// reads the card.
    fn drop_cache(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Device for std::fs::File {
    fn sync(&mut self) -> io::Result<()> {
        self.sync_all()
    }
}

/// The Wi-Fi network the Pi joins on its first boot.
#[derive(Clone, Debug, PartialEq)]
pub struct Wifi {
    pub ssid: String,
    pub password: String,
    pub country: String,
}

impl Wifi {
    /// Why the settings would not work, if they would not.
    pub fn problem(&self) -> Option<&'static str> {
        let bad = |s: &str| s.contains(['\n', '\r']);
        if self.ssid.is_empty() {
            Some(t("the network name is empty", "网络名称是空的"))
        } else if self.ssid.len() > 32 || bad(&self.ssid) {
            Some(t("the network name is too long", "网络名称太长"))
        } else if !self.password.is_empty() && !(8..=63).contains(&self.password.len()) {
            Some(t(
                "a Wi-Fi password has 8 to 63 characters",
                "Wi-Fi 密码是 8 到 63 个字符",
            ))
        } else if bad(&self.password) {
            Some(t("the password has a line break", "密码里有换行"))
        } else if self.country.len() != 2 || !self.country.bytes().all(|b| b.is_ascii_uppercase()) {
            Some(t(
                "the country is two capital letters, such as CN",
                "国家代码是两个大写字母，如 CN",
            ))
        } else {
            None
        }
    }

    /// `runo-wifi.txt`, read by the Pi on its first boot.
    fn file(&self) -> String {
        format!(
            "ssid={}\npassword={}\ncountry={}\n",
            self.ssid, self.password, self.country
        )
    }
}

/// The device password clients give when they pair; the same rule as
/// `runo daemon password`.
pub const MIN_PASSWORD: usize = 8;

/// Why `password` cannot be the device password, if it cannot.
pub fn password_problem(password: &str) -> Option<&'static str> {
    if password.chars().count() < MIN_PASSWORD {
        Some(t(
            "the device password needs at least 8 characters",
            "设备密码至少 8 个字符",
        ))
    } else if password.contains(['\n', '\r']) {
        Some(t("the password has a line break", "密码里有换行"))
    } else if password.trim_start().starts_with('#') {
        // The Pi skips lines starting with # in runo-password.txt.
        Some(t(
            "the device password cannot start with #",
            "设备密码不能以 # 开头",
        ))
    } else {
        None
    }
}

/// A device password made up here: 12 letters and digits that cannot be
/// mistaken for one another.
pub fn new_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut bytes = [0u8; 12];
    getrandom::fill(&mut bytes).expect("the system gives random bytes");
    bytes
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect()
}

/// What goes on the boot partition for the Pi's first start.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub wifi: Option<Wifi>,
    /// Becomes the device password, so a Pi without a screen can pair.
    pub password: Option<String>,
}

impl Settings {
    fn is_empty(&self) -> bool {
        self.wifi.is_none() && self.password.is_none()
    }
}

/// Only images that read `runo-password.txt` ship this; on others the
/// password would stay on the card as it is.
const PASSWORD_EXAMPLE: &str = "runo-password.txt.example";

/// Adds the files for `settings` to the FAT file system in `fat`.
pub fn add_settings(fat: &mut [u8], settings: &Settings) -> io::Result<()> {
    let fs = fatfs::FileSystem::new(Cursor::new(fat), fatfs::FsOptions::new())?;
    let mut files = Vec::new();
    if let Some(wifi) = &settings.wifi {
        files.push(("runo-wifi.txt", wifi.file()));
    }
    if let Some(password) = &settings.password {
        if fs.root_dir().open_file(PASSWORD_EXAMPLE).is_err() {
            return Err(other(t(
                "this image is too old to take a device password: leave the password empty, or use a newer image",
                "这个镜像太旧，不支持设置设备密码：请把密码留空，或换用新的镜像",
            )));
        }
        files.push(("runo-password.txt", format!("{password}\n")));
    }
    for (name, text) in files {
        let mut file = fs.root_dir().create_file(name)?;
        file.truncate()?;
        file.write_all(text.as_bytes())?;
        file.flush()?;
    }
    fs.unmount()
}

/// A partition in an MBR partition table.
#[derive(Clone, Copy, Debug)]
struct Partition {
    kind: u8,
    start: u64,
    end: u64,
}

fn partitions(mbr: &[u8]) -> Option<Vec<Partition>> {
    if mbr.len() < SECTOR || mbr[510..512] != [0x55, 0xaa] {
        return None;
    }
    let parts = (0..4)
        .map(|i| &mbr[446 + 16 * i..446 + 16 * (i + 1)])
        .filter(|e| e[4] != 0)
        .map(|e| {
            let start = u32::from_le_bytes(e[8..12].try_into().unwrap()) as u64 * SECTOR as u64;
            let len = u32::from_le_bytes(e[12..16].try_into().unwrap()) as u64 * SECTOR as u64;
            Partition {
                kind: e[4],
                start,
                end: start + len,
            }
        })
        .collect();
    Some(parts)
}

fn other(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

/// Takes the uncompressed image and writes it to the device.
pub struct Flasher<'a> {
    dev: &'a mut dyn Device,
    settings: Settings,
    capacity: Option<u64>,
    head: Vec<u8>,
    /// How much of the start is kept back; 0 until the first sector is in.
    head_len: usize,
    head_done: bool,
    chunk: Vec<u8>,
    len: u64,
    hash: Sha256,
    /// The image's length by its partition table, once that is in.
    total: Option<u64>,
    unsynced: u64,
    progress: &'a dyn Fn(u64, u64),
}

impl<'a> Flasher<'a> {
    /// `progress` is told how much of the image is on the card and the
    /// image's length, from when the partition table gives that length.
    pub fn new(
        dev: &'a mut dyn Device,
        settings: Settings,
        capacity: Option<u64>,
        progress: &'a dyn Fn(u64, u64),
    ) -> Self {
        Flasher {
            dev,
            settings,
            capacity,
            head: Vec::new(),
            head_len: 0,
            head_done: false,
            chunk: Vec::with_capacity(CHUNK),
            len: 0,
            hash: Sha256::new(),
            total: None,
            unsynced: 0,
            progress,
        }
    }

    /// Waits for the card, then says how much of the image is on it.
    fn synced(&mut self) -> io::Result<()> {
        self.dev.flush()?;
        self.dev.sync()?;
        self.unsynced = 0;
        if let Some(total) = self.total {
            let written = self.len - self.chunk.len() as u64;
            (self.progress)(written.min(total), total);
        }
        Ok(())
    }

    /// Decides how much to keep back, once the partition table is in.
    fn plan_head(&mut self) -> io::Result<()> {
        let parts = partitions(&self.head);
        let need = parts.iter().flatten().map(|p| p.end).max().unwrap_or(0);
        if need > 0 {
            self.total = Some(need);
        }
        if let Some(capacity) = self.capacity
            && need > capacity
        {
            return Err(other(format!(
                "{} ({} < {})",
                t(
                    "the card is too small for the image",
                    "SD 卡比镜像小，放不下"
                ),
                size(capacity),
                size(need),
            )));
        }
        self.head_len = SECTOR;
        if !self.settings.is_empty() {
            let boot = parts
                .unwrap_or_default()
                .into_iter()
                .find(|p| matches!(p.kind, 0x0b | 0x0c | 0x0e))
                .ok_or_else(|| {
                    other(t(
                        "the image has no boot partition to put the settings on",
                        "镜像里没有引导分区，没法写入设置",
                    ))
                })?;
            if boot.end > MAX_HEAD || boot.start < SECTOR as u64 {
                return Err(other(t(
                    "the image's boot partition is not where it should be",
                    "镜像的引导分区位置不对",
                )));
            }
            self.head_len = boot.end as usize;
        }
        Ok(())
    }

    /// Writes all of the start but the first sector.
    fn close_head(&mut self) -> io::Result<()> {
        if !self.settings.is_empty() {
            let boot = partitions(&self.head)
                .unwrap_or_default()
                .into_iter()
                .find(|p| matches!(p.kind, 0x0b | 0x0c | 0x0e))
                .expect("found in plan_head");
            add_settings(
                &mut self.head[boot.start as usize..boot.end as usize],
                &self.settings,
            )?;
        }
        self.hash.update(&self.head);
        self.dev.seek(SeekFrom::Start(SECTOR as u64))?;
        self.dev.write_all(&self.head[SECTOR..])?;
        self.head_done = true;
        self.synced()
    }

    fn write_chunk(&mut self) -> io::Result<()> {
        self.hash.update(&self.chunk);
        // Disks take whole sectors.
        let padded = self.chunk.len().div_ceil(SECTOR) * SECTOR;
        self.chunk.resize(padded, 0);
        self.dev.write_all(&self.chunk)?;
        self.unsynced += self.chunk.len() as u64;
        self.chunk.clear();
        if self.unsynced >= SYNC_EVERY {
            self.synced()?;
        }
        Ok(())
    }

    /// Writes what is left and the first sector, and waits for the card.
    /// Gives the image's length and its SHA-256 as written.
    pub fn finish(mut self) -> Result<(u64, [u8; 32])> {
        if !self.head_done {
            if self.head_len == 0 || self.head.len() < self.head_len {
                bail!(t("the image ends too early", "镜像不完整"));
            }
            self.close_head()?;
        }
        if !self.chunk.is_empty() {
            self.write_chunk()?;
        }
        self.dev.seek(SeekFrom::Start(0))?;
        self.dev.write_all(&self.head[..SECTOR])?;
        self.synced()?;
        Ok((self.len, self.hash.finalize().into()))
    }
}

impl Write for Flasher<'_> {
    fn write(&mut self, mut buf: &[u8]) -> io::Result<usize> {
        let n = buf.len();
        while !buf.is_empty() {
            if !self.head_done {
                let want = if self.head_len == 0 {
                    SECTOR
                } else {
                    self.head_len
                };
                let take = (want - self.head.len()).min(buf.len());
                self.head.extend_from_slice(&buf[..take]);
                buf = &buf[take..];
                self.len += take as u64;
                if self.head_len == 0 && self.head.len() == SECTOR {
                    self.plan_head()?;
                }
                if self.head_len != 0 && self.head.len() == self.head_len {
                    self.close_head()?;
                }
            } else {
                let take = (CHUNK - self.chunk.len()).min(buf.len());
                self.chunk.extend_from_slice(&buf[..take]);
                buf = &buf[take..];
                self.len += take as u64;
                if self.chunk.len() == CHUNK {
                    self.write_chunk()?;
                }
            }
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Reads the first `len` bytes back and checks they are what was written.
pub fn verify(
    dev: &mut dyn Device,
    len: u64,
    want: [u8; 32],
    progress: &dyn Fn(u64, u64),
) -> Result<()> {
    dev.drop_cache()?;
    dev.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut done = 0u64;
    while done < len {
        let real = (len - done).min(CHUNK as u64) as usize;
        let read = real.div_ceil(SECTOR) * SECTOR;
        dev.read_exact(&mut buf[..read])?;
        hash.update(&buf[..real]);
        done += real as u64;
        progress(done, len);
    }
    let got: [u8; 32] = hash.finalize().into();
    if got != want {
        bail!(t(
            "what was read back from the card is not what was written: the card may be faulty",
            "从 SD 卡读回的内容和写入的不一致：SD 卡可能有问题",
        ));
    }
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// An image like Runo's: a partition table, a FAT boot partition and
    /// a Linux partition, `mib` MiB in all.
    pub fn image(mib: u64) -> Vec<u8> {
        let total = mib << 20;
        let mut img = vec![0u8; total as usize];
        let boot = (1u64 << 20, 4u64 << 20);
        let root = (5u64 << 20, total);
        for (i, (kind, (start, end))) in [(0x0cu8, boot), (0x83u8, root)].into_iter().enumerate() {
            let e = &mut img[446 + 16 * i..446 + 16 * (i + 1)];
            e[4] = kind;
            e[8..12].copy_from_slice(&((start / 512) as u32).to_le_bytes());
            e[12..16].copy_from_slice(&(((end - start) / 512) as u32).to_le_bytes());
        }
        img[510] = 0x55;
        img[511] = 0xaa;
        fatfs::format_volume(
            Cursor::new(&mut img[boot.0 as usize..boot.1 as usize]),
            fatfs::FormatVolumeOptions::new(),
        )
        .unwrap();
        {
            let fat = Cursor::new(&mut img[boot.0 as usize..boot.1 as usize]);
            let fs = fatfs::FileSystem::new(fat, fatfs::FsOptions::new()).unwrap();
            let mut file = fs.root_dir().create_file(PASSWORD_EXAMPLE).unwrap();
            file.write_all(b"# put the device password below\n")
                .unwrap();
        }
        for (i, b) in img[root.0 as usize..].iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        img
    }

    pub fn wifi() -> Wifi {
        Wifi {
            ssid: "家里的网".into(),
            password: "secret-password".into(),
            country: "CN".into(),
        }
    }

    /// A file on the boot partition of `img`, as `image` lays it out.
    pub fn read_boot(img: &[u8], name: &str) -> Option<String> {
        let fat = img[1 << 20..4 << 20].to_vec();
        let fs = fatfs::FileSystem::new(Cursor::new(fat), fatfs::FsOptions::new()).unwrap();
        let mut file = fs.root_dir().open_file(name).ok()?;
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        Some(text)
    }

    pub fn read_wifi(img: &[u8]) -> Option<String> {
        read_boot(img, "runo-wifi.txt")
    }

    fn with_wifi() -> Settings {
        Settings {
            wifi: Some(wifi()),
            password: None,
        }
    }

    fn flash(img: &[u8], settings: Settings, capacity: Option<u64>) -> Result<Vec<u8>> {
        let mut dev = tempfile::tempfile().unwrap();
        let mut flasher = Flasher::new(&mut dev, settings, capacity, &|_, _| {});
        // Odd pieces, as a decompressor gives them.
        for piece in img.chunks(777_777) {
            flasher.write_all(piece)?;
        }
        let (len, hash) = flasher.finish()?;
        assert_eq!(len, img.len() as u64);
        verify(&mut dev, len, hash, &|_, _| {})?;
        let mut out = Vec::new();
        dev.seek(SeekFrom::Start(0)).unwrap();
        dev.read_to_end(&mut out).unwrap();
        Ok(out)
    }

    #[test]
    fn the_image_is_written_as_it_is() {
        let img = image(9);
        assert_eq!(flash(&img, Settings::default(), None).unwrap(), img);
    }

    #[test]
    fn wifi_settings_go_on_the_boot_partition() {
        let img = image(9);
        let out = flash(&img, with_wifi(), None).unwrap();
        assert_eq!(
            read_wifi(&out).unwrap(),
            "ssid=家里的网\npassword=secret-password\ncountry=CN\n"
        );
        assert_eq!(out[4 << 20..], img[4 << 20..]);
        assert_eq!(out[..SECTOR], img[..SECTOR]);
    }

    #[test]
    fn progress_counts_what_is_on_the_card_against_the_partition_table() {
        // Longer than its partitions, as an image may be padded.
        let mut img = image(9);
        img.extend_from_slice(&[0u8; 4096]);
        let total = 9u64 << 20;
        let seen = std::sync::Mutex::new(Vec::new());
        let progress = |done, of| seen.lock().unwrap().push((done, of));
        let mut dev = tempfile::tempfile().unwrap();
        let mut flasher = Flasher::new(&mut dev, with_wifi(), None, &progress);
        for piece in img.chunks(777_777) {
            flasher.write_all(piece).unwrap();
        }
        flasher.finish().unwrap();
        let seen = seen.into_inner().unwrap();
        // The boot partition, held back for the Wi-Fi settings, then the end.
        assert_eq!(seen.first(), Some(&(4 << 20, total)));
        assert_eq!(seen.last(), Some(&(total, total)));
        assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0), "{seen:?}");
    }

    #[test]
    fn the_device_password_goes_on_the_boot_partition() {
        let img = image(9);
        let settings = Settings {
            wifi: Some(wifi()),
            password: Some("pair with me".into()),
        };
        let out = flash(&img, settings, None).unwrap();
        assert_eq!(
            read_boot(&out, "runo-password.txt").unwrap(),
            "pair with me\n"
        );
        assert!(read_wifi(&out).is_some());
        assert_eq!(out[4 << 20..], img[4 << 20..]);
    }

    #[test]
    fn an_image_that_cannot_read_the_password_is_refused() {
        let mut img = image(9);
        {
            let fat = Cursor::new(&mut img[1 << 20..4 << 20]);
            let fs = fatfs::FileSystem::new(fat, fatfs::FsOptions::new()).unwrap();
            fs.root_dir().remove(PASSWORD_EXAMPLE).unwrap();
        }
        let settings = Settings {
            wifi: None,
            password: Some("pair with me".into()),
        };
        let err = flash(&img, settings, None).unwrap_err();
        assert!(err.to_string().contains("too old"), "{err}");
    }

    #[test]
    fn device_passwords_are_checked_and_made_up_well() {
        assert_eq!(password_problem("pair with me"), None);
        for bad in ["short", "two\nlines!", "#comment"] {
            assert!(password_problem(bad).is_some(), "{bad}");
        }
        let a = new_password();
        assert_eq!(a.len(), 12);
        assert_eq!(password_problem(&a), None);
        assert_ne!(a, new_password());
    }

    #[test]
    fn a_card_too_small_is_refused_before_anything_is_written() {
        let img = image(9);
        let err = flash(&img, Settings::default(), Some(8 << 20)).unwrap_err();
        assert!(err.to_string().contains("too small"), "{err}");
    }

    #[test]
    fn a_card_that_reads_back_wrong_is_reported() {
        let img = image(6);
        let mut dev = tempfile::tempfile().unwrap();
        let mut flasher = Flasher::new(&mut dev, Settings::default(), None, &|_, _| {});
        flasher.write_all(&img).unwrap();
        let (len, hash) = flasher.finish().unwrap();
        dev.seek(SeekFrom::Start(3 << 20)).unwrap();
        dev.write_all(b"bad").unwrap();
        assert!(verify(&mut dev, len, hash, &|_, _| {}).is_err());
    }

    #[test]
    fn wifi_settings_are_checked() {
        assert_eq!(wifi().problem(), None);
        let open = Wifi {
            password: String::new(),
            ..wifi()
        };
        assert_eq!(open.problem(), None);
        for bad in [
            Wifi {
                ssid: String::new(),
                ..wifi()
            },
            Wifi {
                password: "short".into(),
                ..wifi()
            },
            Wifi {
                country: "cn".into(),
                ..wifi()
            },
            Wifi {
                ssid: "a\nb".into(),
                ..wifi()
            },
        ] {
            assert!(bad.problem().is_some(), "{bad:?}");
        }
    }
}
