//! Changes to the system's configuration as transactions (Runo 1.0, 4.2).
//! A change snapshots the files it touches, writes them, restarts the
//! services it names and runs its checks; when a step or a check fails,
//! the files and services are put back as they were. A change cut off half
//! way (a crash, a power cut) is put back on the next start. Every change
//! stays in `<state>/config-history/<id>/` with what its files held before
//! and after, so it can be read and reverted later.

use std::fmt::Write as _;
use std::fs::File;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::i18n::{Tr, tr};
use crate::system::{Action, Check, Host, check, run_commands, service_commands, valid_service};

/// The directory under the state directory.
pub const DIR: &str = "config-history";
const RECORD: &str = "change.json";
const LOCK: &str = "lock";
const MAX_FILES: usize = 16;
const MAX_FILE_BYTES: usize = 256 * 1024;
/// Longest wait for the checks to pass after a restart.
pub const MAX_SETTLE: Duration = Duration::from_secs(120);
/// Default wait: long enough for a network to come back up.
pub const DEFAULT_SETTLE: Duration = Duration::from_secs(20);
/// Time between rounds of checks while waiting.
const POLL: Duration = Duration::from_secs(2);
/// What a restart may take.
const SERVICE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a change that may cut the device off waits to be confirmed.
pub const CONFIRM_WINDOW: Duration = Duration::from_secs(120);
/// Names in services and paths that touch the network, the firewall or
/// runod itself: changing them may cut a client off (4.2, item 4).
const CUTS_OFF: &[&str] = &[
    "network",
    "interfaces",
    "wpa_supplicant",
    "dhcpcd",
    "udhcpc",
    "netplan",
    "networkd",
    "resolv",
    "nftables",
    "iptables",
    "firewall",
    "ufw",
    "hostapd",
    "runod",
];
/// Kernel and device trees: not configuration.
const FORBIDDEN: &[&str] = &["/proc", "/sys", "/dev"];

const NONE_YET: Tr = tr("No changes recorded yet.", "还没有配置修改记录。");
const NO_CHANGE: Tr = tr("no change #{}", "没有编号为 #{} 的修改");

#[cfg(test)]
pub const ALL: &[Tr] = &[NONE_YET, NO_CHANGE];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    /// Being applied; if found so on start, it was cut off.
    Pending,
    /// Applied, and its checks passed.
    Applied,
    /// A step or check failed; everything was put back.
    RolledBack,
    /// Putting it back failed too: the log says what is left.
    RollbackFailed,
    /// Applied and checked, but it may have cut the device off: it stays
    /// only if someone confirms it from a client or the device in time
    /// (4.2, item 4); otherwise runod puts it back.
    Unconfirmed,
}

impl State {
    pub fn word(self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Applied => "applied",
            State::RolledBack => "rolled back",
            State::RollbackFailed => "ROLLBACK FAILED",
            State::Unconfirmed => "waiting for confirmation",
        }
    }
}

/// A file's owner and mode, kept to put it back as it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    mode: u32,
    uid: u32,
    gid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRecord {
    /// Where it was written (a symlink's target, not the link).
    pub path: PathBuf,
    /// `None` when the file did not exist.
    pub before: Option<Meta>,
    /// Whether the change leaves a file there.
    pub after: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckSpec {
    pub kind: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub id: u64,
    /// RFC 3339, UTC.
    pub time: String,
    /// Who asked for it: `cli` (the local window or command), an actor id.
    pub actor: String,
    pub summary: String,
    pub files: Vec<FileRecord>,
    pub services: Vec<String>,
    pub checks: Vec<CheckSpec>,
    pub state: State,
    /// The change this one reverts.
    #[serde(default)]
    pub reverts: Option<u64>,
    /// What happened: restarts, checks, putting back.
    #[serde(default)]
    pub log: Vec<String>,
    /// While unconfirmed: the Unix time it is put back unless confirmed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm_by: Option<i64>,
}

impl Record {
    /// Its checks, as they were asked for.
    fn check_list(&self) -> Vec<(Check, String)> {
        self.checks
            .iter()
            .filter_map(|c| Some((Check::parse(&c.kind)?, c.target.clone())))
            .collect()
    }

    /// One line for a list.
    pub fn line(&self) -> String {
        let files: Vec<String> = self
            .files
            .iter()
            .map(|f| f.path.display().to_string())
            .collect();
        format!(
            "#{} {} {} [{}] {}{}",
            self.id,
            self.time.get(..16).unwrap_or(&self.time).replace('T', " "),
            self.actor,
            self.state.word(),
            self.summary,
            if files.is_empty() {
                String::new()
            } else {
                format!(" ({})", files.join(", "))
            }
        )
    }

    /// The whole record as text, for the agent and the terminal.
    pub fn report(&self) -> String {
        let mut out = format!(
            "change #{} {}: {}\n",
            self.id,
            self.state.word(),
            self.summary
        );
        let _ = writeln!(out, "at {} by {}", self.time, self.actor);
        if let Some(id) = self.reverts {
            let _ = writeln!(out, "reverts change #{id}");
        }
        for file in &self.files {
            let what = match (file.before.is_some(), file.after) {
                (false, true) => "created",
                (true, true) => "changed",
                (true, false) => "deleted",
                (false, false) => "left absent",
            };
            let _ = writeln!(out, "file {} ({what})", file.path.display());
        }
        if !self.services.is_empty() {
            let _ = writeln!(out, "services restarted: {}", self.services.join(", "));
        }
        for line in &self.log {
            let _ = writeln!(out, "{line}");
        }
        match self.state {
            State::Applied => {}
            State::RolledBack => out.push_str(
                "Nothing of this change is left: the files and services are as they were before it.\n",
            ),
            State::RollbackFailed => out.push_str(
                "Putting it back failed: the log above says what is left. Fix it by hand or revert again.\n",
            ),
            State::Pending => out.push_str("Still being applied.\n"),
            State::Unconfirmed => out.push_str(
                "It may cut the device off from the client, so it is kept only when confirmed: \
                 the user confirms it (Keep) in the client app, with `runo remote confirm`, or with `runo changes confirm` on the device, \
                 within two minutes; otherwise it is put back by itself.\n",
            ),
        }
        out
    }
}

/// A change to make.
pub struct Request {
    pub summary: String,
    pub actor: String,
    /// Each file's new content; `None` deletes it.
    pub files: Vec<(PathBuf, Option<Vec<u8>>)>,
    /// Restarted, in order, once the files are written.
    pub services: Vec<String>,
    /// All must pass within `settle`, or everything is put back.
    pub checks: Vec<(Check, String)>,
    pub settle: Duration,
    pub reverts: Option<u64>,
    /// Kept only if confirmed within this time (4.2, item 4).
    pub confirm: Option<Duration>,
}

/// Which side of a change to put on disk.
#[derive(Clone, Copy)]
enum Side {
    Before,
    After,
}

impl Side {
    fn dir(self) -> &'static str {
        match self {
            Side::Before => "before",
            Side::After => "after",
        }
    }
}

/// Held while a change is applied, so two never run at once and a change
/// in progress is not mistaken for one that was cut off.
struct Lock(#[allow(dead_code)] File);

pub struct History {
    dir: PathBuf,
    /// Restarts run through the init system; tests have none.
    restarts: bool,
}

impl History {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            restarts: true,
        }
    }

    pub fn in_state(state: &Path) -> Self {
        Self::new(state.join(DIR))
    }

    #[cfg(test)]
    pub fn without_restarts(dir: PathBuf) -> Self {
        Self {
            dir,
            restarts: false,
        }
    }

    fn lock(&self) -> Result<Option<Lock>> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("cannot create {}", self.dir.display()))?;
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(LOCK))?;
        use std::os::fd::AsRawFd;
        // SAFETY: flock on a descriptor this File owns; it is released when
        // the File is closed, also when the process dies.
        let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        Ok(taken.then_some(Lock(file)))
    }

    fn entry(&self, id: u64) -> PathBuf {
        self.dir.join(format!("{id:06}"))
    }

    fn ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str()?.parse().ok())
            .collect();
        ids.sort_unstable();
        ids
    }

    pub fn load(&self, id: u64) -> Result<Record> {
        let path = self.entry(id).join(RECORD);
        let text =
            std::fs::read_to_string(&path).with_context(|| NO_CHANGE.with(&[&id.to_string()]))?;
        serde_json::from_str(&text).with_context(|| format!("cannot read {}", path.display()))
    }

    /// Every change, newest first.
    pub fn list(&self) -> Vec<Record> {
        self.ids()
            .into_iter()
            .rev()
            .filter_map(|id| self.load(id).ok())
            .collect()
    }

    /// The newest `limit` changes, one line each.
    pub fn summary(&self, limit: usize) -> String {
        let records = self.list();
        if records.is_empty() {
            return NONE_YET.now().to_owned();
        }
        records
            .iter()
            .take(limit)
            .map(Record::line)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn save(&self, record: &Record) -> Result<()> {
        let json = serde_json::to_vec_pretty(record)?;
        write_atomic(&self.entry(record.id).join(RECORD), &json, None)
    }

    /// Applies `request` as one change. `Err` when it cannot start (a bad
    /// request, another change running); otherwise the record says whether
    /// it stayed or was put back.
    pub async fn apply(&self, request: Request) -> Result<Record> {
        let files = validate(&request, &self.dir)?;
        let Some(_lock) = self.lock()? else {
            bail!("another change is being applied; try again when it has finished");
        };
        if let Some(waiting) = self.unconfirmed().first() {
            bail!(
                "change #{} is waiting for confirmation: confirm or revert it first",
                waiting.id
            );
        }
        let id = self.ids().last().map_or(1, |id| id + 1);
        let entry = self.entry(id);
        for side in [Side::Before, Side::After] {
            std::fs::create_dir_all(entry.join(side.dir()))?;
        }
        let mut record = Record {
            id,
            time: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            actor: request.actor.clone(),
            summary: request.summary.trim().to_owned(),
            files: Vec::new(),
            services: request.services.clone(),
            checks: request
                .checks
                .iter()
                .map(|(kind, target)| CheckSpec {
                    kind: Check::NAMES[*kind as usize].to_owned(),
                    target: target.clone(),
                })
                .collect(),
            state: State::Pending,
            reverts: request.reverts,
            log: Vec::new(),
            confirm_by: None,
        };
        // The snapshot first, then the record: from here a crash is put back.
        for (index, (path, content)) in files.iter().enumerate() {
            let before = match std::fs::metadata(path) {
                Ok(meta) => {
                    let bytes = std::fs::read(path)
                        .with_context(|| format!("cannot read {}", path.display()))?;
                    write_atomic(&entry.join("before").join(index.to_string()), &bytes, None)?;
                    Some(Meta {
                        mode: meta.permissions().mode() & 0o7777,
                        uid: meta.uid(),
                        gid: meta.gid(),
                    })
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                Err(err) => bail!("cannot read {}: {err}", path.display()),
            };
            if let Some(bytes) = content {
                write_atomic(&entry.join("after").join(index.to_string()), bytes, None)?;
            }
            record.files.push(FileRecord {
                path: path.clone(),
                before,
                after: content.is_some(),
            });
        }
        self.save(&record)?;

        let failed = match self.put(&record, Side::After) {
            Err(err) => Some(format!("writing the files failed: {err:#}")),
            Ok(()) => match self.restart(&mut record).await {
                Some(why) => Some(why),
                None => {
                    let (passed, lines) = verify(&request.checks, request.settle).await;
                    record.log.extend(lines);
                    (!passed).then(|| "a check failed".to_owned())
                }
            },
        };
        match failed {
            None => match request.confirm {
                Some(wait) => {
                    let by = chrono::Utc::now().timestamp() + wait.as_secs() as i64;
                    record.state = State::Unconfirmed;
                    record.confirm_by = Some(by);
                    record.log.push(format!(
                        "waiting for confirmation until {}",
                        chrono::DateTime::from_timestamp(by, 0)
                            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                            .unwrap_or_default()
                    ));
                }
                None => record.state = State::Applied,
            },
            Some(why) => {
                record.log.push(format!("{why}: putting everything back"));
                self.roll_back(&mut record, &request.checks, request.settle)
                    .await;
            }
        }
        self.save(&record)?;
        Ok(record)
    }

    /// Puts the files back as they were before `record`, restarts its
    /// services and checks again how the device is.
    async fn roll_back(&self, record: &mut Record, checks: &[(Check, String)], settle: Duration) {
        match self.put(record, Side::Before) {
            Ok(()) => {
                record.state = State::RolledBack;
                if let Some(why) = self.restart(record).await {
                    record.log.push(why);
                }
                if !checks.is_empty() {
                    record.log.push("after putting it back:".into());
                    let (_, lines) = verify(checks, settle).await;
                    record.log.extend(lines);
                }
            }
            Err(err) => {
                record.state = State::RollbackFailed;
                record
                    .log
                    .push(format!("putting the files back failed: {err:#}"));
            }
        }
    }

    /// Writes one side of a change: each file's content then, or no file.
    fn put(&self, record: &Record, side: Side) -> Result<()> {
        let entry = self.entry(record.id);
        for (index, file) in record.files.iter().enumerate() {
            let (exists, meta) = match side {
                Side::Before => (file.before.is_some(), file.before),
                Side::After => (file.after, file.before),
            };
            if exists {
                let saved = entry.join(side.dir()).join(index.to_string());
                let bytes = std::fs::read(&saved)
                    .with_context(|| format!("the saved copy {} is gone", saved.display()))?;
                write_atomic(&file.path, &bytes, meta.as_ref())
                    .with_context(|| format!("cannot write {}", file.path.display()))?;
            } else {
                match std::fs::remove_file(&file.path) {
                    Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                        return Err(err)
                            .with_context(|| format!("cannot delete {}", file.path.display()));
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// Restarts the record's services in order; the failure, if any.
    async fn restart(&self, record: &mut Record) -> Option<String> {
        if !self.restarts {
            return None;
        }
        let init = Host::live().init();
        for name in record.services.clone() {
            let commands = match service_commands(init, &name, Action::Restart) {
                Ok(commands) => commands,
                Err(err) => return Some(err),
            };
            let ran = run_commands(&commands, SERVICE_TIMEOUT, 4096).await;
            record
                .log
                .push(format!("restart {name}:\n{}", ran.text.trim_end()));
            if !ran.ok {
                return Some(format!("restarting {name} failed"));
            }
        }
        None
    }

    /// Reverts change `id` as a new change: its files go back to what
    /// they held before it, and its services are restarted and checked.
    /// Files changed again since are left alone unless `force`.
    pub async fn revert(&self, id: u64, actor: &str, force: bool) -> Result<Record> {
        let record = self.load(id)?;
        if record.state != State::Applied {
            bail!(
                "change #{id} is {}: nothing of it is left to revert",
                record.state.word()
            );
        }
        let entry = self.entry(id);
        let mut files = Vec::new();
        let mut moved = Vec::new();
        for (index, file) in record.files.iter().enumerate() {
            let now = std::fs::read(&file.path).ok();
            let then = file
                .after
                .then(|| std::fs::read(entry.join("after").join(index.to_string())))
                .transpose()
                .context("the saved copy is gone")?;
            if now != then {
                moved.push(file.path.display().to_string());
            }
            let before = file
                .before
                .is_some()
                .then(|| std::fs::read(entry.join("before").join(index.to_string())))
                .transpose()
                .context("the saved copy is gone")?;
            files.push((file.path.clone(), before));
        }
        if !moved.is_empty() && !force {
            bail!(
                "changed again since change #{id}: {}. Revert the later changes first, or force it",
                moved.join(", ")
            );
        }
        let checks = record
            .services
            .iter()
            .map(|name| (Check::Service, name.clone()))
            .collect();
        self.apply(Request {
            summary: format!("revert #{id}: {}", record.summary),
            actor: actor.to_owned(),
            files,
            services: record.services.clone(),
            checks,
            settle: DEFAULT_SETTLE,
            reverts: Some(id),
            confirm: None,
        })
        .await
    }

    /// The changes waiting for confirmation. No change is made while one
    /// waits, so only the newest can be.
    pub fn unconfirmed(&self) -> Vec<Record> {
        self.ids()
            .last()
            .and_then(|id| self.load(*id).ok())
            .filter(|r| r.state == State::Unconfirmed)
            .into_iter()
            .collect()
    }

    /// Settles change `id`, waiting for confirmation: `keep` confirms it,
    /// otherwise it is put back now.
    pub async fn confirm(&self, id: u64, keep: bool, actor: &str) -> Result<Record> {
        let Some(_lock) = self.lock()? else {
            bail!("another change is being applied; try again when it has finished");
        };
        let mut record = self.load(id)?;
        if record.state != State::Unconfirmed {
            bail!(
                "change #{id} is {}, not waiting for confirmation",
                record.state.word()
            );
        }
        record.confirm_by = None;
        if keep {
            record.state = State::Applied;
            record.log.push(format!("confirmed by {actor}"));
        } else {
            record
                .log
                .push(format!("{actor} turned it down: putting everything back"));
            let checks = record.check_list();
            self.roll_back(&mut record, &checks, DEFAULT_SETTLE).await;
        }
        self.save(&record)?;
        Ok(record)
    }

    /// Puts back the changes whose time to be confirmed ran out by `now`
    /// (Unix time); nothing while another change is being applied.
    pub async fn expire(&self, now: i64) -> Vec<Record> {
        let due: Vec<Record> = self
            .unconfirmed()
            .into_iter()
            .filter(|r| r.confirm_by.is_none_or(|by| by <= now))
            .collect();
        if due.is_empty() {
            return due;
        }
        let Ok(Some(_lock)) = self.lock() else {
            return Vec::new();
        };
        let mut expired = Vec::new();
        for mut record in due {
            record.confirm_by = None;
            record
                .log
                .push("not confirmed in time: putting everything back".into());
            let checks = record.check_list();
            self.roll_back(&mut record, &checks, DEFAULT_SETTLE).await;
            if let Err(err) = self.save(&record) {
                eprintln!("config history: cannot save change #{}: {err:#}", record.id);
            }
            expired.push(record);
        }
        expired
    }

    /// Puts back every change that was cut off before it finished. Nothing
    /// happens while another process is applying one.
    pub async fn recover(&self) -> Vec<Record> {
        if !self.dir.is_dir() {
            return Vec::new();
        }
        let Ok(Some(_lock)) = self.lock() else {
            return Vec::new();
        };
        let mut recovered = Vec::new();
        for id in self.ids() {
            let Ok(mut record) = self.load(id) else {
                continue;
            };
            if record.state != State::Pending {
                continue;
            }
            record
                .log
                .push("cut off before it finished: put back on the next start".into());
            self.roll_back(&mut record, &[], Duration::ZERO).await;
            if let Err(err) = self.save(&record) {
                eprintln!("config history: cannot save change #{id}: {err:#}");
            }
            recovered.push(record);
        }
        recovered
    }
}

/// Whether a change to these files and services may cut a client off.
pub fn may_cut_off(files: &[PathBuf], services: &[String]) -> bool {
    let touches = |text: &str| {
        let text = text.to_lowercase();
        CUTS_OFF.iter().any(|word| text.contains(word))
    };
    services.iter().any(|s| touches(s)) || files.iter().any(|f| touches(&f.to_string_lossy()))
}

/// The files a request may write, symlinks resolved; `Err` says why not.
fn validate(request: &Request, history: &Path) -> Result<Vec<(PathBuf, Option<Vec<u8>>)>> {
    if request.summary.trim().is_empty() {
        bail!("say in the summary what the change is for");
    }
    if request.files.is_empty() && request.services.is_empty() {
        bail!("a change needs files to write or services to restart");
    }
    if request.files.len() > MAX_FILES {
        bail!("at most {MAX_FILES} files in one change");
    }
    for name in &request.services {
        if !valid_service(name) {
            bail!("{name:?} is not a service name");
        }
    }
    let mut files: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();
    for (path, content) in &request.files {
        if !path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        {
            bail!(
                "{} must be an absolute path without . or ..",
                path.display()
            );
        }
        if content.as_ref().is_some_and(|c| c.len() > MAX_FILE_BYTES) {
            bail!("{} is over {} KiB", path.display(), MAX_FILE_BYTES / 1024);
        }
        // A link (like /etc/resolv.conf) is followed: its target changes.
        let resolved = match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => std::fs::canonicalize(path)
                .with_context(|| format!("{} links to nothing", path.display()))?,
            Ok(meta) if !meta.is_file() => bail!("{} is not a regular file", path.display()),
            _ => path.clone(),
        };
        if FORBIDDEN.iter().any(|f| resolved.starts_with(f)) || resolved.starts_with(history) {
            bail!("{} is not configuration", resolved.display());
        }
        if files.iter().any(|(p, _)| *p == resolved) {
            bail!("{} is named twice", resolved.display());
        }
        files.push((resolved, content.clone()));
    }
    Ok(files)
}

/// Runs the checks until all pass or `settle` is over.
async fn verify(checks: &[(Check, String)], settle: Duration) -> (bool, Vec<String>) {
    let deadline = Instant::now() + settle.min(MAX_SETTLE);
    let mut results: Vec<Option<String>> = vec![None; checks.len()];
    loop {
        for ((kind, target), result) in checks.iter().zip(results.iter_mut()) {
            if result.as_deref().is_some_and(|r| r.starts_with("ok:")) {
                continue;
            }
            *result = Some(check(*kind, target).await);
        }
        let passed = results
            .iter()
            .all(|r| r.as_deref().is_some_and(|r| r.starts_with("ok:")));
        if passed || Instant::now() >= deadline {
            let lines = checks
                .iter()
                .zip(results)
                .map(|((kind, target), result)| {
                    format!(
                        "check {} {target}: {}",
                        Check::NAMES[*kind as usize],
                        result.unwrap_or_default()
                    )
                })
                .collect();
            return (passed, lines);
        }
        tokio::time::sleep(POLL.min(deadline.saturating_duration_since(Instant::now()))).await;
    }
}

/// Writes `path` through a temporary file and a rename, so it is never
/// half written; with `meta`, the old owner and mode are kept.
fn write_atomic(path: &Path, bytes: &[u8], meta: Option<&Meta>) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no directory", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .with_context(|| format!("{} has no file name", path.display()))?;
    let temp = parent.join(format!(".{}.runo-new", name.to_string_lossy()));
    let mut file = File::create(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    let mode = meta.map_or(0o644, |m| m.mode);
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(mode))?;
    if let Some(meta) = meta
        && crate::elevate::is_root()
    {
        std::os::unix::fs::chown(&temp, Some(meta.uid), Some(meta.gid))?;
    }
    std::fs::rename(&temp, path)?;
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(summary: &str, files: Vec<(PathBuf, Option<&str>)>) -> Request {
        Request {
            summary: summary.into(),
            actor: "cli".into(),
            files: files
                .into_iter()
                .map(|(p, c)| (p, c.map(|c| c.as_bytes().to_vec())))
                .collect(),
            services: Vec::new(),
            checks: Vec::new(),
            settle: Duration::ZERO,
            reverts: None,
            confirm: None,
        }
    }

    #[test]
    fn network_firewall_and_runod_changes_may_cut_off() {
        let path = |p: &str| vec![PathBuf::from(p)];
        assert!(may_cut_off(&path("/etc/wpa_supplicant.conf"), &[]));
        assert!(may_cut_off(&path("/etc/nftables.conf"), &[]));
        assert!(may_cut_off(&[], &["S40network".into()]));
        assert!(may_cut_off(&[], &["NetworkManager".into()]));
        assert!(may_cut_off(&[], &["runod".into()]));
        assert!(!may_cut_off(&path("/etc/timezone"), &["S48time".into()]));
        assert!(!may_cut_off(&path("/etc/ntp.conf"), &[]));
    }

    #[tokio::test]
    async fn a_change_waiting_for_confirmation_is_kept_or_put_back() {
        let dir = tempfile::tempdir().unwrap();
        let history = History::without_restarts(dir.path().join(DIR));
        let conf = dir.path().join("wpa_supplicant.conf");
        std::fs::write(&conf, "old\n").unwrap();
        let waiting = |content: &'static str| Request {
            confirm: Some(Duration::from_secs(120)),
            ..request("wifi", vec![(conf.clone(), Some(content))])
        };

        let first = history.apply(waiting("new\n")).await.unwrap();
        assert_eq!(first.state, State::Unconfirmed);
        assert!(first.report().contains("confirm"));
        assert_eq!(std::fs::read_to_string(&conf).unwrap(), "new\n");
        // Nothing else until it is settled.
        let err = history.apply(waiting("other\n")).await.unwrap_err();
        assert!(format!("{err}").contains("#1 is waiting"));
        // Not yet due.
        assert!(
            history
                .expire(chrono::Utc::now().timestamp())
                .await
                .is_empty()
        );
        let kept = history.confirm(first.id, true, "client:abc").await.unwrap();
        assert_eq!(kept.state, State::Applied);
        assert!(kept.confirm_by.is_none());
        assert!(history.confirm(first.id, true, "cli").await.is_err());

        let second = history.apply(waiting("newer\n")).await.unwrap();
        let expired = history.expire(second.confirm_by.unwrap()).await;
        assert_eq!(expired.len(), 1);
        assert_eq!(history.load(second.id).unwrap().state, State::RolledBack);
        assert_eq!(std::fs::read_to_string(&conf).unwrap(), "new\n");

        let third = history.apply(waiting("newest\n")).await.unwrap();
        let refused = history.confirm(third.id, false, "cli").await.unwrap();
        assert_eq!(refused.state, State::RolledBack);
        assert_eq!(std::fs::read_to_string(&conf).unwrap(), "new\n");
        assert!(history.unconfirmed().is_empty());
    }

    #[tokio::test]
    async fn a_change_stays_when_its_checks_pass_and_can_be_reverted() {
        let dir = tempfile::tempdir().unwrap();
        let history = History::without_restarts(dir.path().join(DIR));
        let conf = dir.path().join("etc/ntp.conf");
        let new = dir.path().join("etc/new.conf");
        std::fs::create_dir_all(conf.parent().unwrap()).unwrap();
        std::fs::write(&conf, "server old\n").unwrap();
        std::fs::set_permissions(&conf, std::fs::Permissions::from_mode(0o600)).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut req = request(
            "use the pool",
            vec![
                (conf.clone(), Some("server pool\n")),
                (new.clone(), Some("x\n")),
            ],
        );
        req.checks = vec![(Check::Tcp, listener.local_addr().unwrap().to_string())];
        let record = history.apply(req).await.unwrap();
        assert_eq!(record.state, State::Applied, "{}", record.report());
        assert_eq!(std::fs::read_to_string(&conf).unwrap(), "server pool\n");
        // The owner's mode is kept.
        assert_eq!(
            std::fs::metadata(&conf).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "x\n");
        let report = record.report();
        assert!(
            report.contains("change #1 applied: use the pool"),
            "{report}"
        );
        assert!(report.contains("ntp.conf (changed)"), "{report}");
        assert!(report.contains("new.conf (created)"), "{report}");
        assert!(report.contains("check tcp 127.0.0.1:"), "{report}");
        assert_eq!(history.list().len(), 1);
        assert!(history.summary(10).contains("#1 "));

        let reverted = history.revert(1, "cli", false).await.unwrap();
        assert_eq!(reverted.state, State::Applied);
        assert_eq!(reverted.reverts, Some(1));
        assert_eq!(std::fs::read_to_string(&conf).unwrap(), "server old\n");
        assert!(!new.exists());
        assert_eq!(history.list()[0].id, 2);
        // Reverting the revert brings the change back.
        history.revert(2, "cli", false).await.unwrap();
        assert_eq!(std::fs::read_to_string(&conf).unwrap(), "server pool\n");
    }

    #[tokio::test]
    async fn a_failed_check_puts_everything_back() {
        let dir = tempfile::tempdir().unwrap();
        let history = History::without_restarts(dir.path().join(DIR));
        let conf = dir.path().join("wpa.conf");
        let new = dir.path().join("sub/created.conf");
        std::fs::write(&conf, "old\n").unwrap();
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().to_string()
        };
        let mut req = request(
            "break it",
            vec![(conf.clone(), Some("new\n")), (new.clone(), Some("y\n"))],
        );
        req.checks = vec![(Check::Tcp, closed)];
        let record = history.apply(req).await.unwrap();
        assert_eq!(record.state, State::RolledBack);
        assert_eq!(std::fs::read_to_string(&conf).unwrap(), "old\n");
        assert!(!new.exists());
        let report = record.report();
        assert!(
            report.contains("a check failed: putting everything back"),
            "{report}"
        );
        assert!(
            report.contains("Nothing of this change is left"),
            "{report}"
        );
        // Nothing to revert in a change that was put back.
        assert!(history.revert(record.id, "cli", false).await.is_err());
    }

    #[tokio::test]
    async fn a_change_cut_off_is_put_back_on_the_next_start() {
        let dir = tempfile::tempdir().unwrap();
        let history = History::without_restarts(dir.path().join(DIR));
        let conf = dir.path().join("hostname");
        std::fs::write(&conf, "pi\n").unwrap();
        let record = history
            .apply(request("rename", vec![(conf.clone(), Some("runo\n"))]))
            .await
            .unwrap();
        // As if the power went right after the files were written.
        let mut cut = record.clone();
        cut.state = State::Pending;
        history.save(&cut).unwrap();
        // Not while another process holds the lock.
        let lock = history.lock().unwrap().unwrap();
        assert!(history.recover().await.is_empty());
        drop(lock);
        let recovered = history.recover().await;
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].state, State::RolledBack);
        assert_eq!(std::fs::read_to_string(&conf).unwrap(), "pi\n");
        assert!(history.recover().await.is_empty());
        assert!(
            History::new(dir.path().join("none"))
                .recover()
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn files_changed_since_are_not_reverted_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let history = History::without_restarts(dir.path().join(DIR));
        let conf = dir.path().join("tz");
        std::fs::write(&conf, "UTC\n").unwrap();
        history
            .apply(request("tz", vec![(conf.clone(), Some("Asia/Shanghai\n"))]))
            .await
            .unwrap();
        std::fs::write(&conf, "Europe/Berlin\n").unwrap();
        let err = history.revert(1, "cli", false).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("changed again since"),
            "{err:#}"
        );
        assert_eq!(std::fs::read_to_string(&conf).unwrap(), "Europe/Berlin\n");
        history.revert(1, "cli", true).await.unwrap();
        assert_eq!(std::fs::read_to_string(&conf).unwrap(), "UTC\n");
    }

    #[tokio::test]
    async fn links_are_followed_and_bad_requests_change_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let history = History::without_restarts(dir.path().join(DIR));
        let target = dir.path().join("real.conf");
        let link = dir.path().join("resolv.conf");
        std::fs::write(&target, "nameserver 1.1.1.1\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let record = history
            .apply(request(
                "dns",
                vec![(link.clone(), Some("nameserver 9.9.9.9\n"))],
            ))
            .await
            .unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "nameserver 9.9.9.9\n"
        );
        assert_eq!(record.files[0].path, target.canonicalize().unwrap());

        let ok = dir.path().join("ok.conf");
        for (req, why) in [
            (request("", vec![(ok.clone(), Some("x"))]), "summary"),
            (request("x", vec![]), "needs files"),
            (request("x", vec![("etc/x".into(), Some("x"))]), "absolute"),
            (
                request("x", vec![("/etc/../x".into(), Some("x"))]),
                "absolute",
            ),
            (
                request("x", vec![("/proc/sys/x".into(), Some("x"))]),
                "not configuration",
            ),
            (
                request(
                    "x",
                    vec![(dir.path().join(DIR).join("000001/change.json"), Some("x"))],
                ),
                "not configuration",
            ),
            (
                request("x", vec![(ok.clone(), Some("x")), (ok.clone(), None)]),
                "named twice",
            ),
            (
                request("x", vec![(dir.path().to_owned(), Some("x"))]),
                "not a regular file",
            ),
            (
                Request {
                    services: vec!["a;b".into()],
                    ..request("x", vec![])
                },
                "not a service name",
            ),
        ] {
            let err = history.apply(req).await.unwrap_err();
            assert!(format!("{err:#}").contains(why), "{why}: {err:#}");
        }
        assert!(!ok.exists());
        assert_eq!(history.list().len(), 1);
    }

    #[test]
    fn every_text_is_translated() {
        crate::i18n::assert_complete(ALL);
    }

    #[tokio::test]
    async fn only_one_change_runs_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let history = History::without_restarts(dir.path().join(DIR));
        let _held = history.lock().unwrap().unwrap();
        let err = history
            .apply(request("x", vec![(dir.path().join("a"), Some("a"))]))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("another change"), "{err:#}");
    }
}
