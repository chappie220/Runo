//! Updates of the Runo system (4.5): the whole image, A/B. A signed bundle
//! is checked before anything is written; its root file system goes to
//! the slot not running, its kernel under the boot partition's `a/` or
//! `b/`; the Pi then boots it once with `tryboot`. There runod checks the
//! new system and only then makes it the default; anything else (a failed
//! check, a hang the watchdog ends, a crash) boots the old slot again.
//!
//! A bundle is a tar of `manifest.json`, `manifest.sig` (Ed25519 over the
//! manifest's bytes, by a key in [`KEYS`]), `rootfs.ext4.gz` and
//! `boot.tar.gz` (the kernel, device trees and overlays).

use std::fs::File;
use std::io::{BufReader, Read, Seek, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use aws_lc_rs::digest;
use serde::{Deserialize, Serialize};

use crate::i18n::{Tr, tr};

/// The release keys updates must be signed with: one hex Ed25519 public
/// key per line. Built into the image; none means no updates.
pub const KEYS: &str = "/etc/runo/update-keys";
const SYSTEM: &str = "/etc/runo-system";
/// The update being tried, kept across the reboot.
const PENDING: &str = "system-update.json";
/// What the last update came to.
const RESULT: &str = "system-update-result.json";
const BOOT: &str = "/boot";
/// The disk signature of the image (genimage.cfg.in): partitions are
/// `PARTUUID=52554e4f-0N`.
const SIGNATURE: &str = "52554e4f";
const ROOTFS: &str = "rootfs.ext4.gz";
const BOOT_FILES: &str = "boot.tar.gz";
const MAX_MANIFEST: u64 = 64 * 1024;
const MAX_BUNDLE: u64 = 2 * 1024 * 1024 * 1024;
/// How long the new system may take to find its network.
const NETWORK_WAIT: Duration = Duration::from_secs(90);

const CHECKING: Tr = tr("Checking {} ...", "正在校验 {} ...");
const WRITING: Tr = tr(
    "Writing Runo {} to slot {} ...",
    "正在把 Runo {} 写入 {} 槽 ...",
);
const REBOOTING: Tr = tr(
    "Written and checked. Restarting into the new system once; it stays only if it starts well.",
    "已写入并校验。现在重启进入新系统试运行一次；它正常启动后才会保留。",
);
const SAME: Tr = tr(
    "Runo {} is already installed; --force installs it again.",
    "已经是 Runo {}；加 --force 可重新安装。",
);
pub const NOT_SYSTEM: Tr = tr(
    "System updates are for the Runo system image; elsewhere `runo update` updates the program.",
    "系统更新只用于 Runo 系统镜像；其他系统上用 `runo update` 更新程序。",
);
const NO_KEYS: Tr = tr(
    "This image was built without a release key, so it takes no system updates: flash a new image instead.",
    "这个镜像构建时没有发布密钥，因此不能做系统更新：请重新烧录新镜像。",
);
const S_INSTALLED: Tr = tr("Updated to Runo {} on {}.", "已更新到 Runo {}（{}）。");
const S_FAILED: Tr = tr(
    "The update to Runo {} on {} was put back: {}",
    "更新到 Runo {}（{}）失败，已回到原系统：{}",
);
const S_SYSTEM: Tr = tr("Runo {} ({}, slot {})", "Runo {}（{}，{} 槽）");

#[cfg(test)]
pub const ALL: &[Tr] = &[
    CHECKING,
    WRITING,
    REBOOTING,
    SAME,
    NOT_SYSTEM,
    NO_KEYS,
    S_INSTALLED,
    S_FAILED,
    S_SYSTEM,
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSum {
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub board: String,
    pub version: String,
    /// The bundle's other files.
    pub files: std::collections::BTreeMap<String, FileSum>,
    /// The root file system as written to the slot (uncompressed).
    pub rootfs: FileSum,
}

/// A slot: its letter (the boot partition's directory) and partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Slot {
    A,
    B,
}

impl Slot {
    pub fn letter(self) -> &'static str {
        match self {
            Slot::A => "a",
            Slot::B => "b",
        }
    }

    pub fn partition(self) -> u32 {
        match self {
            Slot::A => 2,
            Slot::B => 3,
        }
    }

    pub fn other(self) -> Slot {
        match self {
            Slot::A => Slot::B,
            Slot::B => Slot::A,
        }
    }

    fn partuuid(self) -> String {
        format!("{SIGNATURE}-{:02}", self.partition())
    }
}

/// The slot the kernel's command line booted.
pub fn running_slot(cmdline: &str) -> Option<Slot> {
    cmdline.split_whitespace().find_map(|arg| {
        match arg.strip_prefix("root=PARTUUID=")?.to_lowercase() {
            p if p == Slot::A.partuuid() => Some(Slot::A),
            p if p == Slot::B.partuuid() => Some(Slot::B),
            _ => None,
        }
    })
}

/// `board=` and `version=` of /etc/runo-system.
pub fn system_info() -> Option<(String, String)> {
    let text = std::fs::read_to_string(SYSTEM).ok()?;
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{name}=")))
            .map(|v| v.trim().to_owned())
    };
    Some((field("board")?, field("version").unwrap_or_default()))
}

#[cfg(test)]
fn sha256_hex(data: &[u8]) -> String {
    runo_link::hex::encode(digest::digest(&digest::SHA256, data).as_ref())
}

/// A reader that hashes what passes through it.
struct Hashing<R> {
    inner: R,
    context: digest::Context,
    size: u64,
}

impl<R: Read> Hashing<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            context: digest::Context::new(&digest::SHA256),
            size: 0,
        }
    }

    fn finish(self) -> FileSum {
        FileSum {
            size: self.size,
            sha256: runo_link::hex::encode(self.context.finish().as_ref()),
        }
    }
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.context.update(&buf[..n]);
        self.size += n as u64;
        Ok(n)
    }
}

/// The trusted keys in `text`: one hex key per line, `#` comments.
pub fn parse_keys(text: &str) -> Vec<[u8; 32]> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter_map(|l| runo_link::hex::decode(l)?.try_into().ok())
        .collect()
}

/// Reads a bundle once: its manifest, checked against `keys`, and every
/// file's checksum against it. Nothing is written.
pub fn verify(bundle: &Path, keys: &[[u8; 32]]) -> Result<Manifest> {
    if keys.is_empty() {
        bail!("{}", NO_KEYS.now());
    }
    let file = File::open(bundle).with_context(|| format!("opening {}", bundle.display()))?;
    if file.metadata()?.len() > MAX_BUNDLE {
        bail!("{} is too large to be an update", bundle.display());
    }
    let mut archive = tar::Archive::new(BufReader::new(file));
    let mut manifest_bytes = None;
    let mut signature = None;
    let mut sums = std::collections::BTreeMap::new();
    for entry in archive.entries()? {
        let entry = entry?;
        let name = entry.path()?.to_string_lossy().into_owned();
        match name.as_str() {
            "manifest.json" | "manifest.sig" => {
                let mut bytes = Vec::new();
                entry.take(MAX_MANIFEST).read_to_end(&mut bytes)?;
                if name == "manifest.json" {
                    manifest_bytes = Some(bytes);
                } else {
                    signature = Some(bytes);
                }
            }
            _ => {
                let mut hashing = Hashing::new(entry);
                std::io::copy(&mut hashing, &mut std::io::sink())?;
                sums.insert(name, hashing.finish());
            }
        }
    }
    let manifest_bytes = manifest_bytes.context("the bundle has no manifest.json")?;
    let signature = signature.context("the bundle is not signed (no manifest.sig)")?;
    let signed = keys.iter().any(|key| {
        aws_lc_rs::signature::UnparsedPublicKey::new(&aws_lc_rs::signature::ED25519, key)
            .verify(&manifest_bytes, &signature)
            .is_ok()
    });
    if !signed {
        bail!("the bundle is not signed by a Runo release key");
    }
    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).context("reading manifest.json")?;
    for name in [ROOTFS, BOOT_FILES] {
        let want = manifest
            .files
            .get(name)
            .with_context(|| format!("the manifest does not list {name}"))?;
        let got = sums
            .get(name)
            .with_context(|| format!("the bundle has no {name}"))?;
        if want != got {
            bail!("{name} does not match the manifest: the bundle is damaged");
        }
    }
    Ok(manifest)
}

/// Where an update goes: the boot partition, and the device the other
/// slot's root file system is written to.
pub struct Layout {
    pub boot: PathBuf,
    pub running: Slot,
    pub target: PathBuf,
}

/// Writes the root file system of `bundle` to `layout.target` and reads
/// it back; then the kernel files to `<boot>/<slot>/`, and `tryboot.txt`
/// to boot them once.
pub fn write(bundle: &Path, manifest: &Manifest, layout: &Layout) -> Result<()> {
    let slot = layout.running.other();
    let mut archive = tar::Archive::new(BufReader::new(File::open(bundle)?));
    let mut wrote_rootfs = false;
    let mut wrote_boot = false;
    for entry in archive.entries()? {
        let entry = entry?;
        let name = entry.path()?.to_string_lossy().into_owned();
        if name == ROOTFS {
            write_rootfs(entry, manifest, &layout.target)?;
            wrote_rootfs = true;
        } else if name == BOOT_FILES {
            write_boot(entry, &layout.boot, slot, layout.running)?;
            wrote_boot = true;
        }
    }
    if !wrote_rootfs || !wrote_boot {
        bail!("the bundle changed while it was written");
    }
    // Last: from here the firmware may boot the new slot, once.
    let config = std::fs::read_to_string(layout.boot.join("config.txt"))
        .context("reading config.txt on the boot partition")?;
    write_file(
        &layout.boot.join("tryboot.txt"),
        with_prefix(&config, slot).as_bytes(),
    )?;
    Ok(())
}

fn write_rootfs(entry: impl Read, manifest: &Manifest, target: &Path) -> Result<()> {
    let mut device = File::options()
        .write(true)
        .open(target)
        .with_context(|| format!("opening {}", target.display()))?;
    let room = device.seek(std::io::SeekFrom::End(0))?;
    device.seek(std::io::SeekFrom::Start(0))?;
    // A regular file (tests) has no fixed size.
    let is_file = device.metadata()?.is_file();
    if !is_file && manifest.rootfs.size > room {
        bail!(
            "the new system ({} MB) does not fit its partition ({} MB)",
            manifest.rootfs.size >> 20,
            room >> 20
        );
    }
    let mut source = Hashing::new(flate2::read::GzDecoder::new(entry));
    let written = std::io::copy(&mut source, &mut device)?;
    device.sync_all()?;
    let sum = source.finish();
    if sum != manifest.rootfs || written != manifest.rootfs.size {
        bail!("the written system does not match the manifest");
    }
    // Read back what the device holds.
    let mut back = Hashing::new(File::open(target)?.take(manifest.rootfs.size));
    std::io::copy(&mut back, &mut std::io::sink())?;
    if back.finish() != manifest.rootfs {
        bail!(
            "reading back {} gave other bytes: the card may be failing",
            target.display()
        );
    }
    Ok(())
}

/// A relative path with nothing but normal parts.
fn plain(path: &Path) -> bool {
    path.components().all(|c| matches!(c, Component::Normal(_)))
}

fn write_boot(entry: impl Read, boot: &Path, slot: Slot, running: Slot) -> Result<()> {
    let fresh = boot.join(format!("{}.new", slot.letter()));
    let _ = std::fs::remove_dir_all(&fresh);
    std::fs::create_dir_all(&fresh)?;
    let mut files = tar::Archive::new(flate2::read::GzDecoder::new(entry));
    for file in files.entries()? {
        let mut file = file?;
        let path = file.path()?.into_owned();
        let path = path.strip_prefix("./").unwrap_or(&path).to_path_buf();
        if path.as_os_str().is_empty() {
            continue;
        }
        if !plain(&path) {
            bail!("boot.tar.gz holds a path outside it: {}", path.display());
        }
        let to = fresh.join(&path);
        if file.header().entry_type().is_dir() {
            std::fs::create_dir_all(&to)?;
            continue;
        }
        if !file.header().entry_type().is_file() {
            continue;
        }
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&to)?;
        std::io::copy(&mut file, &mut out)?;
        out.sync_all()?;
    }
    if !fresh.join("Image").exists() && !fresh.join("kernel8.img").exists() {
        bail!("boot.tar.gz has no kernel");
    }
    // The running slot's command line, pointing at the new slot.
    let cmdline = std::fs::read_to_string(boot.join(running.letter()).join("cmdline.txt"))
        .context("reading the running slot's cmdline.txt")?;
    let cmdline = cmdline.replace(
        &format!("PARTUUID={}", running.partuuid()),
        &format!("PARTUUID={}", slot.partuuid()),
    );
    write_file(&fresh.join("cmdline.txt"), cmdline.as_bytes())?;
    let old = boot.join(slot.letter());
    let _ = std::fs::remove_dir_all(&old);
    std::fs::rename(&fresh, &old)?;
    Ok(())
}

/// `config.txt` with its `os_prefix` set to `slot`.
pub fn with_prefix(config: &str, slot: Slot) -> String {
    let line = format!("os_prefix={}/", slot.letter());
    let mut found = false;
    let mut out: Vec<String> = config
        .lines()
        .map(|l| {
            if l.trim_start().starts_with("os_prefix=") {
                found = true;
                line.clone()
            } else {
                l.to_owned()
            }
        })
        .collect();
    if !found {
        out.push(line);
    }
    out.join("\n") + "\n"
}

/// Writes `path` whole or not at all.
fn write_file(path: &Path, data: &[u8]) -> Result<()> {
    let temp = path.with_extension("tmp");
    let mut file = File::create(&temp).with_context(|| format!("writing {}", temp.display()))?;
    file.write_all(data)?;
    file.sync_all()?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

/// The update being tried, and what it came to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub version: String,
    pub from: Slot,
    pub to: Slot,
    /// Unix time it was written.
    pub at: i64,
    /// Whether the old system had a network; the new one must find one.
    pub network: bool,
    #[serde(default)]
    pub ok: Option<bool>,
    #[serde(default)]
    pub why: Option<String>,
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// The last update's outcome, for the status page.
pub fn status_line(state: &Path, lang: crate::i18n::Lang) -> Option<String> {
    let mut line = system_info().map(|(board, version)| {
        let slot = std::fs::read_to_string("/proc/cmdline")
            .ok()
            .and_then(|c| running_slot(&c))
            .map_or("?", Slot::letter);
        S_SYSTEM.fill(lang, &[&version, &board, slot])
    })?;
    if let Some(attempt) = read_json::<Attempt>(&state.join(RESULT)) {
        let when = crate::daemon::cli::date(attempt.at);
        let text = match attempt.ok {
            Some(true) => S_INSTALLED.fill(lang, &[&attempt.version, &when]),
            _ => S_FAILED.fill(
                lang,
                &[
                    &attempt.version,
                    &when,
                    attempt.why.as_deref().unwrap_or_default(),
                ],
            ),
        };
        line = format!("{line}; {text}");
    }
    Some(line)
}

fn has_default_route() -> bool {
    std::fs::read_to_string("/proc/net/route")
        .map(|t| {
            t.lines()
                .skip(1)
                .any(|l| l.split_whitespace().nth(1) == Some("00000000"))
        })
        .unwrap_or(false)
        || std::fs::read_to_string("/proc/net/ipv6_route")
            .map(|t| {
                t.lines()
                    .any(|l| l.starts_with("00000000000000000000000000000000 00"))
            })
            .unwrap_or(false)
}

/// Whether the Pi booted with `tryboot` this time.
fn tryboot() -> bool {
    std::fs::read("/proc/device-tree/chosen/bootloader/tryboot")
        .is_ok_and(|v| v.last().is_some_and(|b| *b == 1))
}

/// The block device of partition `number` on the disk holding `/`.
fn partition_device(number: u32) -> Result<PathBuf> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")?;
    let root = mountinfo
        .lines()
        .find(|l| l.split_whitespace().nth(4) == Some("/"))
        .and_then(|l| l.split_whitespace().nth(2))
        .context("finding the root file system's device")?;
    let sys = std::fs::canonicalize(format!("/sys/dev/block/{root}"))?;
    let disk = sys.parent().context("the root device has no disk")?;
    for entry in std::fs::read_dir(disk)? {
        let entry = entry?;
        let number_file = entry.path().join("partition");
        if std::fs::read_to_string(&number_file).is_ok_and(|n| n.trim() == number.to_string()) {
            return Ok(Path::new("/dev").join(entry.file_name()));
        }
    }
    bail!("partition {number} of {} not found", disk.display())
}

fn remount_boot(writable: bool) -> Result<()> {
    let mode = if writable { "remount,rw" } else { "remount,ro" };
    let status = std::process::Command::new("mount")
        .args(["-o", mode, BOOT])
        .status()
        .context("running mount")?;
    if !status.success() {
        bail!("cannot remount {BOOT} ({mode})");
    }
    Ok(())
}

/// Stops the services, then restarts the Pi into the other slot once
/// (`tryboot`), or normally.
fn reboot(tryboot: bool) -> Result<()> {
    // Stopping the services stops runod, which hangs up its terminal
    // sessions, the one this may run in among them: carry on regardless,
    // or the Pi is left with nothing running and no restart.
    // SAFETY: plain signal(2) with SIG_IGN.
    unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
    if Path::new("/etc/init.d/rcK").exists() {
        let _ = std::process::Command::new("/etc/init.d/rcK").status();
    }
    let _ = std::process::Command::new("mount")
        .args(["-o", "remount,ro", "/data"])
        .status();
    // SAFETY: sync has no preconditions.
    unsafe { libc::sync() };
    let arg = std::ffi::CString::new(if tryboot { "0 tryboot" } else { "0" })?;
    // SAFETY: the documented magic numbers and a NUL-terminated argument.
    let result = unsafe {
        libc::syscall(
            libc::SYS_reboot,
            libc::LINUX_REBOOT_MAGIC1,
            libc::LINUX_REBOOT_MAGIC2,
            libc::LINUX_REBOOT_CMD_RESTART2,
            arg.as_ptr(),
        )
    };
    if result != 0 {
        bail!("restarting failed: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// `runo update` on the Runo system: installs the bundle at `source` (a file or an
/// https URL), or the latest release's for this board.
pub async fn update(
    state: &Path,
    source: Option<&str>,
    force: bool,
    config: &crate::update::UpdateConfig,
) -> Result<()> {
    if !crate::config::runo_system() {
        bail!("{}", NOT_SYSTEM.now());
    }
    if !is_root() {
        bail!("a system update needs root");
    }
    let (board, version) = system_info().context("reading /etc/runo-system")?;
    let keys = parse_keys(&std::fs::read_to_string(KEYS).unwrap_or_default());
    if keys.is_empty() {
        bail!("{}", NO_KEYS.now());
    }
    let download = state.join("system-update.bundle");
    let bundle = match source {
        Some(path) if !path.starts_with("https://") => PathBuf::from(path),
        Some(url) => {
            fetch(url, &download).await?;
            download.clone()
        }
        None => {
            let release = crate::update::latest(config).await?;
            let name = format!("runo-{board}.update");
            let url = release
                .url(&name)
                .with_context(|| format!("release {} has no {name}", release.version))?
                .to_owned();
            if !force && !crate::update::newer(&release.version, &version) {
                println!("{}", SAME.with(&[&version]));
                return Ok(());
            }
            fetch(&url, &download).await?;
            download.clone()
        }
    };
    let result = install(state, &bundle, &keys, &board, &version, force).await;
    let _ = std::fs::remove_file(&download);
    result
}

async fn fetch(url: &str, to: &Path) -> Result<()> {
    use futures_util::StreamExt;
    println!("{}", CHECKING.with(&[url]));
    let response = crate::net::builder()
        .connect_timeout(Duration::from_secs(20))
        .build()?
        .get(url)
        .send()
        .await
        .with_context(|| format!("cannot download {url}"))?;
    if !response.status().is_success() {
        bail!("{url} answered HTTP {}", response.status());
    }
    let mut file = tokio::fs::File::create(to).await?;
    let mut stream = response.bytes_stream();
    let mut size = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("download interrupted")?;
        size += chunk.len() as u64;
        if size > MAX_BUNDLE {
            bail!("{url} is too large to be an update");
        }
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
    }
    tokio::io::AsyncWriteExt::flush(&mut file).await?;
    Ok(())
}

async fn install(
    state: &Path,
    bundle: &Path,
    keys: &[[u8; 32]],
    board: &str,
    version: &str,
    force: bool,
) -> Result<()> {
    println!("{}", CHECKING.with(&[&bundle.display().to_string()]));
    let manifest = verify(bundle, keys)?;
    if manifest.board != board {
        bail!("the bundle is for {}, this is {board}", manifest.board);
    }
    if !force && manifest.version == version {
        println!("{}", SAME.with(&[version]));
        return Ok(());
    }
    let cmdline = std::fs::read_to_string("/proc/cmdline")?;
    let running = running_slot(&cmdline).context("the running slot is unknown")?;
    let slot = running.other();
    let layout = Layout {
        boot: PathBuf::from(BOOT),
        running,
        target: partition_device(slot.partition())?,
    };
    println!("{}", WRITING.with(&[&manifest.version, slot.letter()]));
    remount_boot(true)?;
    let written = write(bundle, &manifest, &layout);
    let _ = remount_boot(false);
    written?;
    let attempt = Attempt {
        version: manifest.version.clone(),
        from: running,
        to: slot,
        at: chrono::Utc::now().timestamp(),
        network: has_default_route(),
        ok: None,
        why: None,
    };
    write_file(&state.join(PENDING), &serde_json::to_vec_pretty(&attempt)?)?;
    let _ = std::fs::remove_file(state.join(RESULT));
    println!("{}", REBOOTING.now());
    if let Err(err) = reboot(true) {
        let _ = std::fs::remove_file(state.join(PENDING));
        return Err(err);
    }
    Ok(())
}

/// What runod does at start when an update is being tried.
#[derive(Debug, PartialEq, Eq)]
pub enum AfterBoot {
    Nothing,
    /// The new slot checked out and is the default now.
    Kept(String),
    /// It did not: back to the old slot.
    PutBack(String),
}

/// The new system's own check: what must work for it to stay.
async fn self_check(attempt: &Attempt, state: &Path) -> Result<()> {
    if !is_mounted("/data") {
        bail!("/data is not mounted");
    }
    if !state.is_dir() {
        bail!("{} is missing", state.display());
    }
    let runo = std::process::Command::new("/usr/bin/runo")
        .arg("--version")
        .output()
        .context("running runo")?;
    if !runo.status.success() {
        bail!("runo does not start");
    }
    if attempt.network {
        let deadline = std::time::Instant::now() + NETWORK_WAIT;
        while !has_default_route() {
            if std::time::Instant::now() > deadline {
                bail!("no network within {} s", NETWORK_WAIT.as_secs());
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
    Ok(())
}

/// Finishes an update after the reboot, from runod: keeps the new slot
/// when it checks out, otherwise goes back to the old one.
pub async fn after_boot(state: &Path) -> AfterBoot {
    let Some(mut attempt) = read_json::<Attempt>(&state.join(PENDING)) else {
        return AfterBoot::Nothing;
    };
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let running = running_slot(&cmdline);
    let outcome = if running == Some(attempt.to) {
        match self_check(&attempt, state).await {
            Ok(()) => commit(Path::new(BOOT), attempt.to).map_err(|e| format!("{e:#}")),
            Err(err) => Err(format!("the new system failed its check: {err:#}")),
        }
    } else if tryboot() {
        Err("the new system did not boot into its slot".into())
    } else {
        Err("the new system did not start, or this Pi's firmware does not support tryboot (update its bootloader)".into())
    };
    let _ = std::fs::remove_file(state.join(PENDING));
    let _ = remount_boot(true);
    let _ = std::fs::remove_file(Path::new(BOOT).join("tryboot.txt"));
    let _ = remount_boot(false);
    attempt.ok = Some(outcome.is_ok());
    attempt.why = outcome.as_ref().err().cloned();
    if let Ok(json) = serde_json::to_vec_pretty(&attempt) {
        let _ = write_file(&state.join(RESULT), &json);
    }
    match outcome {
        Ok(()) => AfterBoot::Kept(attempt.version),
        Err(why) => {
            // Still on the new slot: go back to the old one now.
            if running == Some(attempt.to) {
                let _ = reboot(false);
            }
            AfterBoot::PutBack(why)
        }
    }
}

/// Makes `slot` the default: `config.txt` names it.
fn commit(boot: &Path, slot: Slot) -> Result<()> {
    remount_boot(true)?;
    let result = (|| {
        let config = std::fs::read_to_string(boot.join("config.txt"))?;
        write_file(
            &boot.join("config.txt"),
            with_prefix(&config, slot).as_bytes(),
        )
    })();
    let _ = remount_boot(false);
    result
}

/// Feeds the hardware watchdog while runod runs (4.5): a hung system
/// restarts by itself, and a new slot that hangs goes back to the old.
pub struct Watchdog(Option<File>);

impl Watchdog {
    /// Opens /dev/watchdog on the Runo system; elsewhere does nothing.
    pub fn start() -> Self {
        if !crate::config::runo_system() {
            return Watchdog(None);
        }
        match File::options().write(true).open("/dev/watchdog") {
            Ok(file) => Watchdog(Some(file)),
            Err(err) => {
                crate::daemon::log(&format!("watchdog: {err}"));
                Watchdog(None)
            }
        }
    }

    pub fn active(&self) -> bool {
        self.0.is_some()
    }

    pub fn feed(&mut self) {
        if let Some(file) = &mut self.0 {
            let _ = file.write_all(b"\0");
        }
    }

    /// A clean stop: the magic close, so the watchdog does not fire.
    pub fn stop(mut self) {
        if let Some(mut file) = self.0.take() {
            let _ = file.write_all(b"V");
        }
    }
}

/// Whether something is mounted at `path`.
fn is_mounted(path: &str) -> bool {
    std::fs::read_to_string("/proc/mounts")
        .map(|mounts| {
            mounts
                .lines()
                .any(|line| line.split_whitespace().nth(1) == Some(path))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};

    #[test]
    fn every_text_is_translated() {
        crate::i18n::assert_complete(ALL);
    }

    #[test]
    fn the_running_slot_comes_from_the_root_partition() {
        assert_eq!(
            running_slot("console=tty1 root=PARTUUID=52554e4f-02 rootwait ro"),
            Some(Slot::A)
        );
        assert_eq!(running_slot("root=PARTUUID=52554E4F-03"), Some(Slot::B));
        assert_eq!(running_slot("root=/dev/mmcblk0p2"), None);
        assert_eq!(Slot::A.other(), Slot::B);
    }

    #[test]
    fn the_prefix_is_replaced_or_added() {
        let config = "arm_64bit=1\nos_prefix=a/\nkernel=Image\n";
        assert_eq!(
            with_prefix(config, Slot::B),
            "arm_64bit=1\nos_prefix=b/\nkernel=Image\n"
        );
        assert_eq!(
            with_prefix("kernel=Image", Slot::A),
            "kernel=Image\nos_prefix=a/\n"
        );
    }

    #[test]
    fn keys_are_read_one_per_line() {
        let key = "ab".repeat(32);
        let keys = parse_keys(&format!("# release\n{key}\n\nnot a key\n{key} # again\n"));
        assert_eq!(keys, vec![[0xab; 32], [0xab; 32]]);
    }

    struct Built {
        bundle: PathBuf,
        rootfs: Vec<u8>,
        key: [u8; 32],
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn tar_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    /// A bundle signed with a fresh key; `tamper` changes it after signing.
    fn bundle(dir: &Path, tamper: impl Fn(&mut Manifest, &mut Vec<u8>)) -> Built {
        let rootfs: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let rootfs_gz = gzip(&rootfs);
        let boot = gzip(&tar_of(&[
            ("Image", b"kernel"),
            ("bcm2712-rpi-5-b.dtb", b"dtb"),
            ("overlays/disable-bt.dtbo", b"overlay"),
        ]));
        let sum = |d: &[u8]| FileSum {
            size: d.len() as u64,
            sha256: sha256_hex(d),
        };
        let mut manifest = Manifest {
            board: "rpi5".into(),
            version: "9.9.9".into(),
            files: [
                (ROOTFS.to_owned(), sum(&rootfs_gz)),
                (BOOT_FILES.to_owned(), sum(&boot)),
            ]
            .into(),
            rootfs: sum(&rootfs),
        };
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&aws_lc_rs::rand::SystemRandom::new()).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let json = serde_json::to_vec(&manifest).unwrap();
        let signature = pair.sign(&json).as_ref().to_vec();
        let mut rootfs_part = rootfs_gz.clone();
        tamper(&mut manifest, &mut rootfs_part);
        let json_after = serde_json::to_vec(&manifest).unwrap();
        let path = dir.join("runo-rpi5.update");
        std::fs::write(
            &path,
            tar_of(&[
                ("manifest.json", &json_after),
                ("manifest.sig", &signature),
                (ROOTFS, &rootfs_part),
                (BOOT_FILES, &boot),
            ]),
        )
        .unwrap();
        Built {
            bundle: path,
            rootfs,
            key: pair.public_key().as_ref().try_into().unwrap(),
        }
    }

    #[test]
    fn a_signed_bundle_is_written_to_the_other_slot_with_tryboot() {
        let dir = tempfile::tempdir().unwrap();
        let built = bundle(dir.path(), |_, _| {});
        let manifest = verify(&built.bundle, &[built.key]).unwrap();
        assert_eq!(manifest.version, "9.9.9");

        let boot = dir.path().join("boot");
        std::fs::create_dir_all(boot.join("a")).unwrap();
        std::fs::write(boot.join("config.txt"), "arm_64bit=1\nos_prefix=a/\n").unwrap();
        std::fs::write(
            boot.join("a/cmdline.txt"),
            "console=tty1 root=PARTUUID=52554e4f-02 rootwait ro\n",
        )
        .unwrap();
        let target = dir.path().join("p3");
        std::fs::write(&target, vec![0xffu8; 400_000]).unwrap();
        let layout = Layout {
            boot: boot.clone(),
            running: Slot::A,
            target: target.clone(),
        };
        write(&built.bundle, &manifest, &layout).unwrap();
        let disk = std::fs::read(&target).unwrap();
        assert_eq!(&disk[..built.rootfs.len()], &built.rootfs[..]);
        assert_eq!(std::fs::read(boot.join("b/Image")).unwrap(), b"kernel");
        assert!(boot.join("b/overlays/disable-bt.dtbo").exists());
        assert_eq!(
            std::fs::read_to_string(boot.join("b/cmdline.txt")).unwrap(),
            "console=tty1 root=PARTUUID=52554e4f-03 rootwait ro\n"
        );
        assert_eq!(
            std::fs::read_to_string(boot.join("tryboot.txt")).unwrap(),
            "arm_64bit=1\nos_prefix=b/\n"
        );
        // The default stays the running slot until the new one checks out.
        assert!(
            std::fs::read_to_string(boot.join("config.txt"))
                .unwrap()
                .contains("os_prefix=a/")
        );
    }

    #[test]
    fn unsigned_damaged_or_foreign_bundles_are_refused_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let good = bundle(dir.path(), |_, _| {});
        assert!(format!("{:#}", verify(&good.bundle, &[]).unwrap_err()).contains("release key"));
        let other = [7u8; 32];
        assert!(
            format!("{:#}", verify(&good.bundle, &[other]).unwrap_err()).contains("not signed by")
        );

        let changed = bundle(dir.path(), |m, _| m.version = "10.0.0".into());
        assert!(verify(&changed.bundle, &[changed.key]).is_err());

        let damaged = bundle(dir.path(), |_, rootfs| {
            let last = rootfs.len() - 20;
            rootfs[last] ^= 0xff;
        });
        let err = verify(&damaged.bundle, &[damaged.key]).unwrap_err();
        assert!(format!("{err:#}").contains("damaged"), "{err:#}");
    }

    #[test]
    fn boot_files_cannot_leave_their_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(plain(Path::new("overlays/x.dtbo")));
        assert!(!plain(Path::new("../config.txt")));
        assert!(!plain(Path::new("/etc/passwd")));
        let _ = dir;
    }
}
