//! The user dictionary: words the user chose, with how often and how
//! recently, and which word followed which (for association).
//!
//! Stored as lines of tab-separated text, written to a temporary file and
//! renamed over the old one:
//!
//! ```text
//! w <TAB> <count> <TAB> <stamp> <TAB> <word>
//! p <TAB> <count> <TAB> <stamp> <TAB> <previous> <TAB> <word>
//! ```

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};

/// How many words and pairs are kept; the least recently used go first.
pub const CAP: usize = 100_000;

#[derive(Debug, Clone, Copy, Default)]
pub struct Use {
    pub count: u32,
    /// When it was last used, by the dictionary's own clock.
    pub stamp: u64,
}

pub struct User {
    path: Option<PathBuf>,
    pub(super) words: HashMap<String, Use>,
    /// Previous word → the words that followed it.
    pub(super) pairs: HashMap<String, Vec<(String, Use)>>,
    pair_count: usize,
    clock: u64,
    dirty: bool,
    pub(super) cap: usize,
}

/// Whether `text` can be stored in a line.
fn storable(text: &str) -> bool {
    !text.is_empty() && !text.contains(['\t', '\n', '\r'])
}

impl User {
    /// Reads the dictionary at `path`, creating an empty one when missing.
    pub fn load(path: Option<PathBuf>) -> Result<Self> {
        let mut user = User {
            path,
            words: HashMap::new(),
            pairs: HashMap::new(),
            pair_count: 0,
            clock: 0,
            dirty: false,
            cap: CAP,
        };
        let Some(path) = user.path.clone() else {
            return Ok(user);
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => user.parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                user.dirty = true;
                user.save()?;
            }
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
        Ok(user)
    }

    fn parse(&mut self, text: &str) {
        for line in text.lines() {
            let mut fields = line.split('\t');
            let (Some(kind), Some(count), Some(stamp)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let (Ok(count), Ok(stamp)) = (count.parse(), stamp.parse()) else {
                continue;
            };
            let entry = Use { count, stamp };
            self.clock = self.clock.max(stamp);
            match (kind, fields.next(), fields.next()) {
                ("w", Some(word), None) if storable(word) => {
                    self.words.insert(word.to_string(), entry);
                }
                ("p", Some(previous), Some(word)) if storable(previous) && storable(word) => {
                    let next = self.pairs.entry(previous.to_string()).or_default();
                    if !next.iter().any(|(w, _)| w == word) {
                        next.push((word.to_string(), entry));
                        self.pair_count += 1;
                    }
                }
                _ => {}
            }
        }
        self.evict();
    }

    pub fn len(&self) -> usize {
        self.words.len() + self.pair_count
    }

    /// How often `word` was chosen.
    pub fn count(&self, word: &str) -> u32 {
        self.words.get(word).map_or(0, |u| u.count)
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Records that `word` was chosen after `previous`. Returns whether
    /// entries were evicted (so word indexes must be rebuilt).
    pub fn learn(&mut self, previous: Option<&str>, word: &str) -> bool {
        if !storable(word) {
            return false;
        }
        let stamp = self.tick();
        let entry = self.words.entry(word.to_string()).or_default();
        entry.count = entry.count.saturating_add(1);
        entry.stamp = stamp;
        if let Some(previous) = previous.filter(|p| storable(p)) {
            let stamp = self.tick();
            let next = self.pairs.entry(previous.to_string()).or_default();
            match next.iter_mut().find(|(w, _)| w == word) {
                Some((_, entry)) => {
                    entry.count = entry.count.saturating_add(1);
                    entry.stamp = stamp;
                }
                None => {
                    next.push((word.to_string(), Use { count: 1, stamp }));
                    self.pair_count += 1;
                }
            }
        }
        self.dirty = true;
        self.evict()
    }

    /// Words that followed `previous`, most often and most recent first.
    pub fn follow(&self, previous: &str, limit: usize) -> Vec<String> {
        let Some(next) = self.pairs.get(previous) else {
            return Vec::new();
        };
        let mut next: Vec<_> = next.iter().collect();
        next.sort_by_key(|(_, u)| (std::cmp::Reverse(u.count), std::cmp::Reverse(u.stamp)));
        next.into_iter()
            .take(limit)
            .map(|(w, _)| w.clone())
            .collect()
    }

    /// Drops the least recently used entries once over the cap, down to
    /// 99% of it so this does not run on every word.
    fn evict(&mut self) -> bool {
        let len = self.len();
        if len <= self.cap {
            return false;
        }
        let keep = self.cap - self.cap / 100;
        let mut stamps: Vec<u64> = self.words.values().map(|u| u.stamp).collect();
        stamps.extend(self.pairs.values().flatten().map(|(_, u)| u.stamp));
        stamps.sort_unstable();
        let oldest = stamps[len - keep - 1];
        self.words.retain(|_, u| u.stamp > oldest);
        self.pair_count = 0;
        for next in self.pairs.values_mut() {
            next.retain(|(_, u)| u.stamp > oldest);
            self.pair_count += next.len();
        }
        self.pairs.retain(|_, next| !next.is_empty());
        self.dirty = true;
        true
    }

    /// Writes the dictionary when it changed.
    pub fn save(&mut self) -> Result<()> {
        let Some(path) = &self.path else {
            self.dirty = false;
            return Ok(());
        };
        if !self.dirty {
            return Ok(());
        }
        let mut lines: Vec<(u64, String)> = Vec::with_capacity(self.len());
        for (word, u) in &self.words {
            lines.push((u.stamp, format!("w\t{}\t{}\t{word}\n", u.count, u.stamp)));
        }
        for (previous, next) in &self.pairs {
            for (word, u) in next {
                let line = format!("p\t{}\t{}\t{previous}\t{word}\n", u.count, u.stamp);
                lines.push((u.stamp, line));
            }
        }
        lines.sort_unstable();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let tmp = path.with_extension("tmp");
        let write = || -> std::io::Result<()> {
            let mut file = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
            file.write_all(b"# Runo pinyin user dictionary\n")?;
            for (_, line) in &lines {
                file.write_all(line.as_bytes())?;
            }
            file.into_inner()?.sync_all()?;
            std::fs::rename(&tmp, path)
        };
        write().with_context(|| format!("cannot write {}", path.display()))?;
        self.dirty = false;
        Ok(())
    }
}
