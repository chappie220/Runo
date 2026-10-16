//! The SD cards and USB drives the image can go on.
//!
//! Only removable disks (SD, MMC and USB) are listed, and never the one
//! the computer runs from.

#[derive(Clone, Debug, PartialEq)]
pub struct Disk {
    /// What the system calls it: /dev/sdb, or \\.\PhysicalDrive2.
    pub path: String,
    /// Maker and model, as the disk says.
    pub name: String,
    pub size: u64,
    /// USB, SD, ...
    pub bus: String,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub number: u32,
}

#[cfg(target_os = "linux")]
pub use linux::{after, list, needs_admin, open};
#[cfg(windows)]
pub use windows::{after, list, needs_admin, open};

#[cfg(not(any(target_os = "linux", windows)))]
pub fn list() -> anyhow::Result<Vec<Disk>> {
    anyhow::bail!(crate::text::t(
        "writing cards is done on Linux and Windows",
        "写卡只支持 Linux 和 Windows",
    ))
}
#[cfg(not(any(target_os = "linux", windows)))]
pub fn open(_: &Disk) -> anyhow::Result<Box<dyn crate::write::Device>> {
    list().map(|_| unreachable!())
}
#[cfg(not(any(target_os = "linux", windows)))]
pub fn after(_: &Disk) {}
#[cfg(not(any(target_os = "linux", windows)))]
pub fn needs_admin() -> bool {
    false
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::CString;
    use std::fs::{self, File, OpenOptions};
    use std::io::{self, Read, Seek, SeekFrom, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    use anyhow::{Context, Result, bail};

    use super::Disk;
    use crate::text::t;
    use crate::write::Device;

    /// Mount points that mean the disk is the computer's own.
    const SYSTEM_MOUNTS: &[&str] = &["/", "/boot", "/boot/efi", "/efi", "/usr", "/var", "/home"];

    const BLKRRPART: libc::c_ulong = 0x125f;
    const BLKFLSBUF: libc::c_ulong = 0x1261;

    pub fn needs_admin() -> bool {
        // SAFETY: no preconditions.
        unsafe { libc::geteuid() != 0 }
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path)
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    /// (device, mount point) of each mounted file system.
    fn mounts() -> Vec<(String, String)> {
        read(Path::new("/proc/mounts"))
            .lines()
            .filter_map(|line| {
                let mut f = line.split_whitespace();
                let dev = f.next()?.to_string();
                let at = f.next()?.replace("\\040", " ");
                Some((dev, at))
            })
            .collect()
    }

    /// Whether `dev` is the disk `disk` (/dev/sdb) or one of its partitions.
    fn on_disk(dev: &str, disk: &str) -> bool {
        let Some(rest) = dev.strip_prefix(disk) else {
            return false;
        };
        let rest = rest.strip_prefix('p').unwrap_or(rest);
        rest.bytes().all(|b| b.is_ascii_digit())
    }

    pub fn list() -> Result<Vec<Disk>> {
        let mounts = mounts();
        let mut disks = Vec::new();
        for entry in fs::read_dir("/sys/block").context("/sys/block")? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if ["loop", "ram", "zram", "dm-", "md", "sr", "nbd", "fd"]
                .iter()
                .any(|p| name.starts_with(p))
                || name.contains("boot")
                || name.contains("rpmb")
            {
                continue;
            }
            let sys = Path::new("/sys/block").join(&name);
            let real = fs::canonicalize(&sys)
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let usb = real.contains("/usb");
            let mmc = name.starts_with("mmcblk");
            if !(usb || mmc || read(&sys.join("removable")) == "1") {
                continue;
            }
            let size = read(&sys.join("size")).parse::<u64>().unwrap_or(0) * 512;
            if size == 0 {
                // A card reader with no card in it.
                continue;
            }
            let path = format!("/dev/{name}");
            if mounts
                .iter()
                .any(|(dev, at)| on_disk(dev, &path) && SYSTEM_MOUNTS.contains(&at.as_str()))
            {
                continue;
            }
            let model = [
                read(&sys.join("device/vendor")),
                read(&sys.join("device/model")),
                read(&sys.join("device/name")),
            ]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
            let bus = if mmc {
                "SD"
            } else if usb {
                "USB"
            } else {
                "removable"
            };
            disks.push(Disk {
                path,
                name: model,
                size,
                bus: bus.into(),
                number: 0,
            });
        }
        disks.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(disks)
    }

    struct BlockDevice(File);

    impl Read for BlockDevice {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf)
        }
    }
    impl Write for BlockDevice {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }
    impl Seek for BlockDevice {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.0.seek(pos)
        }
    }
    impl Device for BlockDevice {
        fn sync(&mut self) -> io::Result<()> {
            self.0.sync_all()
        }
        fn drop_cache(&mut self) -> io::Result<()> {
            let fd = self.0.as_raw_fd();
            // SAFETY: fd is open; the ioctl takes no argument.
            if unsafe { libc::ioctl(fd, BLKFLSBUF as _, 0) } != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fd is open.
            unsafe { libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_DONTNEED) };
            Ok(())
        }
    }

    /// Unmounts the disk's file systems and opens it for writing.
    pub fn open(disk: &Disk) -> Result<Box<dyn Device>> {
        for (dev, at) in mounts().into_iter().rev() {
            if !on_disk(&dev, &disk.path) {
                continue;
            }
            let c = CString::new(at.clone())?;
            // SAFETY: c is a valid C string.
            if unsafe { libc::umount2(c.as_ptr(), 0) } != 0 {
                let err = io::Error::last_os_error();
                bail!("{} {at}: {err}", t("cannot unmount", "无法卸载"));
            }
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            // Fails while anything still has it mounted.
            .custom_flags(libc::O_EXCL)
            .open(&disk.path)
            .with_context(|| format!("{} {}", t("cannot open", "打不开"), disk.path))?;
        Ok(Box::new(BlockDevice(file)))
    }

    /// Lets the system see the new partitions.
    pub fn after(disk: &Disk) {
        if let Ok(file) = File::open(&disk.path) {
            // SAFETY: fd is open; the ioctl takes no argument.
            unsafe { libc::ioctl(file.as_raw_fd(), BLKRRPART as _, 0) };
        }
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn partitions_belong_to_their_disk() {
            assert!(super::on_disk("/dev/sdb1", "/dev/sdb"));
            assert!(super::on_disk("/dev/sdb", "/dev/sdb"));
            assert!(super::on_disk("/dev/mmcblk0p2", "/dev/mmcblk0"));
            assert!(!super::on_disk("/dev/sdba1", "/dev/sdb"));
            assert!(!super::on_disk("/dev/sdb1", "/dev/sda"));
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::fs::{File, OpenOptions};
    use std::io::{self, Read, Seek, SeekFrom, Write};
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    use anyhow::{Context, Result, bail};
    use serde::Deserialize;

    use super::Disk;
    use crate::text::t;
    use crate::write::Device;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    pub fn needs_admin() -> bool {
        // SAFETY: no preconditions.
        unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() == 0 }
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Listed {
        number: u32,
        name: Option<String>,
        size: u64,
        bus: String,
    }

    const LIST: &str = "[Console]::OutputEncoding = [Text.Encoding]::UTF8; \
        ConvertTo-Json -Compress -InputObject @(Get-Disk | \
        Where-Object { -not $_.IsBoot -and -not $_.IsSystem -and @('USB','SD','MMC') -contains \"$($_.BusType)\" } | \
        ForEach-Object { [pscustomobject]@{ Number = $_.Number; Name = $_.FriendlyName; Size = [uint64]$_.Size; Bus = \"$($_.BusType)\" } })";

    pub fn list() -> Result<Vec<Disk>> {
        let out = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", LIST])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .context("powershell")?;
        if !out.status.success() {
            bail!("Get-Disk: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let text = text.trim().trim_start_matches('\u{feff}');
        let listed: Vec<Listed> = if text.is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(text).context("Get-Disk")?
        };
        Ok(listed
            .into_iter()
            .filter(|d| d.size > 0)
            .map(|d| Disk {
                path: format!(r"\\.\PhysicalDrive{}", d.number),
                name: d.name.unwrap_or_default(),
                size: d.size,
                bus: d.bus,
                number: d.number,
            })
            .collect())
    }

    fn diskpart(script: &str) -> Result<()> {
        let path = std::env::temp_dir().join("runo-flash-diskpart.txt");
        std::fs::write(&path, script)?;
        let out = Command::new("diskpart")
            .arg("/s")
            .arg(&path)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .context("diskpart")?;
        let _ = std::fs::remove_file(&path);
        if !out.status.success() {
            bail!("diskpart: {}", String::from_utf8_lossy(&out.stdout).trim());
        }
        Ok(())
    }

    struct PhysicalDrive(File);

    impl Read for PhysicalDrive {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf)
        }
    }
    impl Write for PhysicalDrive {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }
    impl Seek for PhysicalDrive {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.0.seek(pos)
        }
    }
    impl Device for PhysicalDrive {
        fn sync(&mut self) -> io::Result<()> {
            self.0.sync_all()
        }
    }

    /// Removes the disk's partitions, so Windows lets go of its volumes,
    /// and opens it for writing.
    pub fn open(disk: &Disk) -> Result<Box<dyn Device>> {
        diskpart(&format!("select disk {}\nclean\n", disk.number))
            .context(t("cannot clear the card", "无法清空 SD 卡"))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&disk.path)
            .with_context(|| format!("{} {}", t("cannot open", "打不开"), disk.path))?;
        Ok(Box::new(PhysicalDrive(file)))
    }

    /// Lets Windows see the new partitions.
    pub fn after(_: &Disk) {
        let _ = diskpart("rescan\n");
    }
}
