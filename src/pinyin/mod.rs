//! Pinyin input method: full pinyin with phrases and whole sentences
//! (`nihao`, `xi'an`), abbreviated pinyin (`zg` → 中国, `zhongg`), and a
//! user dictionary that learns chosen words and what follows them.
//!
//! The built-in data is `data/pinyin.txt.gz`, made by
//! `scripts/pinyin-data.py` from pinyin-data (characters) and
//! phrase-pinyin-data's `pinyin.txt` (phrases); see the licenses there.
//! Neither has frequencies, so order comes from how common a character is
//! (its tier and how many phrases use it), whether the reading is its
//! first one, and then what the user chose before.

// The framebuffer console drives this through `Composer`.
#![allow(dead_code)]

mod composer;
#[cfg(test)]
mod tests;
mod user;

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};

#[allow(unused_imports)]
pub use composer::{Composer, Key, Outcome};
use user::User;

static DATA: &[u8] = include_bytes!("data/pinyin.txt.gz");

/// Words longer than this many syllables are not looked for.
const MAX_WORD: usize = 16;

/// A candidate for the start of the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub text: String,
    /// How many bytes of the input it covers.
    pub consumed: usize,
}

pub struct Engine {
    /// Every toneless syllable in the data, sorted; a syllable's id is its
    /// index, so the syllables sharing a prefix are a range of ids.
    names: Vec<String>,
    /// Characters, most common first.
    chars: Vec<char>,
    /// The syllables of `chars[i]`: `char_syl[char_at[i]..char_at[i + 1]]`.
    char_syl: Vec<u16>,
    char_at: Vec<u32>,
    /// (character, its index in `chars`), by character.
    by_char: Vec<(char, u32)>,
    /// Per syllable, its characters as order keys: tier, then whether it is
    /// a secondary reading, then the index in `chars` (see `ORDER_BITS`).
    sounds: Vec<Vec<u32>>,
    /// Phrases, best first: `text[text_at[i]..text_at[i + 1]]`, with
    /// syllables `syl[syl_at[i]..syl_at[i + 1]]`.
    text: String,
    text_at: Vec<u32>,
    syl: Vec<u16>,
    /// First letter of each entry of `syl`.
    initials: Vec<u8>,
    syl_at: Vec<u32>,
    /// Phrase indexes by their initials, then rank.
    by_key: Vec<u32>,
    user: User,
    /// The user's words by their possible initials.
    user_keys: BTreeMap<Vec<u8>, Vec<Arc<str>>>,
}

const ORDER_BITS: u32 = 22;
const INDEX_MASK: u32 = (1 << ORDER_BITS) - 1;

/// A run of input that stands for one syllable: exactly one, or any
/// starting with it (an initial like `zh`, or an unfinished syllable).
#[derive(Debug, Clone, Copy)]
struct Tok {
    end: usize,
    lo: u16,
    hi: u16,
    abbr: bool,
    letter: u8,
}

/// A word found for a run of tokens.
#[derive(Debug, Clone)]
struct Word {
    text: String,
    end: usize,
    tokens: usize,
    abbr: usize,
    user: u32,
    rank: u32,
}

/// Input position after any apostrophes at `pos`.
fn skip(input: &[u8], mut pos: usize) -> usize {
    while input.get(pos) == Some(&b'\'') {
        pos += 1;
    }
    pos
}

/// The syllable as the data spells it: ü as `v` except after j, q, x, y
/// (where it is `u`), and `lue`/`nue` as `lve`/`nve`.
fn canonical(s: &str) -> String {
    if s.starts_with(['j', 'q', 'x', 'y']) {
        s.replace('v', "u")
    } else if s == "lue" || s == "nue" {
        format!("{}ve", &s[..1])
    } else {
        s.to_string()
    }
}

fn has_vowel(s: &str) -> bool {
    s.contains(['a', 'e', 'i', 'o', 'u', 'v'])
}

impl Engine {
    /// The built-in data, plus the user's dictionary at `user` when given
    /// (created when missing).
    pub fn load(user: Option<PathBuf>) -> Result<Self> {
        let mut text = String::new();
        flate2::read::GzDecoder::new(DATA)
            .read_to_string(&mut text)
            .context("cannot read the pinyin data")?;
        let mut engine = Self::parse(&text)?;
        engine.user = User::load(user)?;
        engine.index_user();
        Ok(engine)
    }

    fn parse(data: &str) -> Result<Self> {
        let Some((chars, phrases)) = data
            .split_once("@chars\n")
            .and_then(|(_, rest)| rest.split_once("@phrases\n"))
        else {
            bail!("the pinyin data has no @chars or @phrases");
        };
        let mut names: Vec<String> = chars
            .lines()
            .filter(|l| *l != "-")
            .flat_map(|l| {
                l.char_indices()
                    .nth(1)
                    .map_or("", |(i, _)| &l[i..])
                    .split(',')
            })
            .map(str::to_string)
            .collect();
        names.sort_unstable();
        names.dedup();
        let id = |names: &[String], s: &str| -> Result<u16> {
            match names.binary_search_by(|n| n.as_str().cmp(s)) {
                Ok(i) => Ok(i as u16),
                Err(_) => bail!("unknown syllable {s} in the pinyin data"),
            }
        };

        let mut engine = Engine {
            sounds: vec![Vec::new(); names.len()],
            names: Vec::new(),
            chars: Vec::new(),
            char_syl: Vec::new(),
            char_at: vec![0],
            by_char: Vec::new(),
            text: String::new(),
            text_at: vec![0],
            syl: Vec::new(),
            initials: Vec::new(),
            syl_at: vec![0],
            by_key: Vec::new(),
            user: User::load(None)?,
            user_keys: BTreeMap::new(),
        };
        let mut tier = 0u32;
        for line in chars.lines() {
            if line == "-" {
                tier += 1;
                continue;
            }
            let mut it = line.chars();
            let Some(c) = it.next() else { continue };
            let index = engine.chars.len() as u32;
            for (n, reading) in it.as_str().split(',').enumerate() {
                let s = id(&names, reading)?;
                engine.char_syl.push(s);
                let order = (tier << (ORDER_BITS + 1)) | (u32::from(n > 0) << ORDER_BITS);
                engine.sounds[s as usize].push(order | index);
            }
            engine.chars.push(c);
            engine.char_at.push(engine.char_syl.len() as u32);
            engine.by_char.push((c, index));
        }
        for list in &mut engine.sounds {
            list.sort_unstable();
            list.shrink_to_fit();
        }
        engine.by_char.sort_unstable();

        for line in phrases.lines() {
            let mut it = line.split(' ');
            let Some(text) = it.next() else { continue };
            for s in it {
                engine.syl.push(id(&names, s)?);
                engine.initials.push(s.as_bytes()[0]);
            }
            engine.text.push_str(text);
            engine.text_at.push(engine.text.len() as u32);
            engine.syl_at.push(engine.syl.len() as u32);
        }
        let mut by_key: Vec<u32> = (0..engine.text_at.len() as u32 - 1).collect();
        by_key.sort_by(|&a, &b| engine.key(a).cmp(engine.key(b)).then(a.cmp(&b)));
        engine.by_key = by_key;
        engine.names = names;
        Ok(engine)
    }

    fn key(&self, phrase: u32) -> &[u8] {
        let i = phrase as usize;
        &self.initials[self.syl_at[i] as usize..self.syl_at[i + 1] as usize]
    }

    fn phrase_text(&self, phrase: u32) -> &str {
        let i = phrase as usize;
        &self.text[self.text_at[i] as usize..self.text_at[i + 1] as usize]
    }

    fn phrase_syl(&self, phrase: u32) -> &[u16] {
        let i = phrase as usize;
        &self.syl[self.syl_at[i] as usize..self.syl_at[i + 1] as usize]
    }

    /// The syllables of character `c`.
    fn readings(&self, c: char) -> &[u16] {
        match self.by_char.binary_search_by(|(x, _)| x.cmp(&c)) {
            Ok(i) => {
                let n = self.by_char[i].1 as usize;
                &self.char_syl[self.char_at[n] as usize..self.char_at[n + 1] as usize]
            }
            Err(_) => &[],
        }
    }

    /// The ids of the syllables starting with `prefix`.
    fn range(&self, prefix: &str) -> (u16, u16) {
        let lo = self.names.partition_point(|n| n.as_str() < prefix);
        let hi = lo + self.names[lo..].partition_point(|n| n.starts_with(prefix));
        (lo as u16, hi as u16)
    }

    /// The tokens that can start at each position of `input`.
    fn lattice(&self, input: &str) -> Vec<Vec<Tok>> {
        let bytes = input.as_bytes();
        let mut lattice = vec![Vec::new(); bytes.len() + 1];
        for (i, toks) in lattice.iter_mut().enumerate().take(bytes.len()) {
            if bytes[i] == b'\'' {
                continue;
            }
            let run = bytes[i..]
                .iter()
                .position(|&b| b == b'\'')
                .map_or(bytes.len(), |n| i + n);
            let letter = bytes[i];
            for end in i + 1..=run.min(i + 6) {
                let s = canonical(&input[i..end]);
                if let Ok(id) = self.names.binary_search(&s)
                    && has_vowel(&s)
                {
                    let id = id as u16;
                    toks.push(Tok {
                        end,
                        lo: id,
                        hi: id + 1,
                        abbr: false,
                        letter,
                    });
                }
            }
            let initial = if matches!(bytes.get(i..i + 2), Some(b"zh" | b"ch" | b"sh")) {
                i + 2
            } else if b"aeiouv".contains(&letter) {
                i
            } else {
                i + 1
            };
            // An initial standing for a whole syllable, unless a vowel
            // follows (then it starts a spelled-out syllable).
            let vowel = bytes.get(initial).is_some_and(|b| b"aeiouv".contains(b));
            if initial > i && !vowel {
                let (lo, hi) = self.range(&input[i..initial]);
                if lo < hi {
                    toks.push(Tok {
                        end: initial,
                        lo,
                        hi,
                        abbr: true,
                        letter,
                    });
                }
            }
            // An unfinished last syllable, like `zhon`.
            if run - i >= 2 && run > initial && !toks.iter().any(|t| t.end == run) {
                let (lo, hi) = self.range(&canonical(&input[i..run]));
                if lo < hi {
                    toks.push(Tok {
                        end: run,
                        lo,
                        hi,
                        abbr: true,
                        letter,
                    });
                }
            }
        }
        lattice
    }

    /// Whether a phrase or user word has initials starting with `key` and
    /// longer than it.
    fn longer(&self, key: &[u8]) -> bool {
        let at = self.by_key.partition_point(|&p| self.key(p) <= key);
        if self
            .by_key
            .get(at)
            .is_some_and(|&p| self.key(p).starts_with(key))
        {
            return true;
        }
        self.user_keys
            .range(key.to_vec()..)
            .find(|(k, _)| k.len() > key.len() || !k.starts_with(key))
            .is_some_and(|(k, _)| k.starts_with(key))
    }

    /// Whether `text` can be read as the syllables of `path`.
    fn fits(&self, text: &str, path: &[Tok]) -> bool {
        let mut chars = text.chars();
        for t in path {
            let Some(c) = chars.next() else { return false };
            if !self.readings(c).iter().any(|&s| (t.lo..t.hi).contains(&s)) {
                return false;
            }
        }
        chars.next().is_none()
    }

    /// Words of two or more syllables starting at `pos`.
    fn words(&self, input: &[u8], lattice: &[Vec<Tok>], pos: usize, out: &mut Vec<Word>) {
        let mut path = Vec::new();
        let mut key = Vec::new();
        self.walk(input, lattice, pos, &mut path, &mut key, out);
    }

    fn walk(
        &self,
        input: &[u8],
        lattice: &[Vec<Tok>],
        pos: usize,
        path: &mut Vec<Tok>,
        key: &mut Vec<u8>,
        out: &mut Vec<Word>,
    ) {
        let pos = skip(input, pos);
        for &t in lattice.get(pos).map_or(&[][..], |v| &v[..]) {
            path.push(t);
            key.push(t.letter);
            if path.len() >= 2 {
                self.matches(path, key, out);
            }
            if path.len() < MAX_WORD && self.longer(key) {
                self.walk(input, lattice, t.end, path, key, out);
            }
            path.pop();
            key.pop();
        }
    }

    /// Phrases and user words read exactly as `path`.
    fn matches(&self, path: &[Tok], key: &[u8], out: &mut Vec<Word>) {
        let end = path.last().map_or(0, |t| t.end);
        let abbr = path.iter().filter(|t| t.abbr).count();
        let lo = self.by_key.partition_point(|&p| self.key(p) < key);
        let hi = self.by_key.partition_point(|&p| self.key(p) <= key);
        for &p in &self.by_key[lo..hi] {
            let syl = self.phrase_syl(p);
            if path.iter().zip(syl).all(|(t, s)| (t.lo..t.hi).contains(s)) {
                let text = self.phrase_text(p).to_string();
                let user = self.user.count(&text);
                out.push(Word {
                    text,
                    end,
                    tokens: path.len(),
                    abbr,
                    user,
                    rank: p,
                });
            }
        }
        for text in self.user_keys.get(key).into_iter().flatten() {
            if self.fits(text, path) {
                let user = self.user.count(text);
                let text = text.to_string();
                out.push(Word {
                    text,
                    end,
                    tokens: path.len(),
                    abbr,
                    user,
                    rank: u32::MAX,
                });
            }
        }
    }

    /// Single characters for `t`, best first.
    fn singles(&self, t: &Tok) -> Vec<Word> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        let tok = [*t];
        for text in self.user_keys.get(&[t.letter][..]).into_iter().flatten() {
            if self.fits(text, &tok) && seen.insert(text.to_string()) {
                out.push(self.single(t, text.to_string(), 0));
            }
        }
        let mut orders: Vec<u32> = (t.lo..t.hi)
            .flat_map(|s| &self.sounds[s as usize])
            .copied()
            .collect();
        orders.sort_unstable();
        for order in orders {
            let text = self.chars[(order & INDEX_MASK) as usize].to_string();
            if seen.insert(text.clone()) {
                out.push(self.single(t, text, order));
            }
        }
        out.sort_by_key(|w| (Reverse(w.user), w.rank));
        out
    }

    /// The best single character for `t`.
    fn best_single(&self, t: &Tok) -> Option<Word> {
        let tok = [*t];
        let user = self
            .user_keys
            .get(&[t.letter][..])
            .into_iter()
            .flatten()
            .filter(|text| self.fits(text, &tok))
            .max_by_key(|text| self.user.count(text));
        if let Some(text) = user {
            return Some(self.single(t, text.to_string(), 0));
        }
        let order = (t.lo..t.hi)
            .filter_map(|s| self.sounds[s as usize].first())
            .min()?;
        Some(self.single(
            t,
            self.chars[(order & INDEX_MASK) as usize].to_string(),
            *order,
        ))
    }

    fn single(&self, t: &Tok, text: String, rank: u32) -> Word {
        let user = self.user.count(&text);
        Word {
            text,
            end: t.end,
            tokens: 1,
            abbr: usize::from(t.abbr),
            user,
            rank,
        }
    }

    /// The best reading of the whole input as a run of words, by the sum
    /// over words of (syllables² - 1), so longer words win, 3.5 less per
    /// abbreviated syllable (so spelled-out syllables win over a phrase
    /// that only fits by abbreviating), up to 0.5 more for phrases with
    /// common characters, and more for words the user chose. Ties go to fewer words, then to longer words first. Returns the
    /// text, its number of words and whether one has two or more syllables.
    fn sentence(&self, input: &[u8], lattice: &[Vec<Tok>]) -> Option<(String, usize, bool)> {
        struct Step {
            score: f32,
            /// Syllables of each word so far.
            lens: Vec<usize>,
            from: usize,
            text: String,
        }
        fn beats(a: &Step, b: &Step) -> bool {
            if (a.score - b.score).abs() > 1e-4 {
                return a.score > b.score;
            }
            a.lens.len() < b.lens.len() || (a.lens.len() == b.lens.len() && a.lens > b.lens)
        }
        let n = input.len();
        let mut best: Vec<Option<Step>> = (0..=n).map(|_| None).collect();
        best[0] = Some(Step {
            score: 0.0,
            lens: Vec::new(),
            from: 0,
            text: String::new(),
        });
        for p in 0..n {
            let Some(base) = &best[p] else { continue };
            let (score, lens) = (base.score, base.lens.clone());
            let q = skip(input, p);
            let mut found = Vec::new();
            self.words(input, lattice, q, &mut found);
            found.extend(lattice[q].iter().filter_map(|t| self.best_single(t)));
            for w in found {
                let mut gain = (w.tokens * w.tokens) as f32 - 1.0 - 3.5 * w.abbr as f32;
                if w.user > 0 {
                    gain += 1.0 + (w.user as f32).ln();
                } else if w.tokens > 1 {
                    gain += 0.5 * (1.0 - w.rank as f32 / self.by_key.len() as f32);
                }
                let mut step = Step {
                    score: score + gain,
                    lens: lens.clone(),
                    from: p,
                    text: w.text,
                };
                step.lens.push(w.tokens);
                if best[w.end].as_ref().is_none_or(|old| beats(&step, old)) {
                    best[w.end] = Some(step);
                }
            }
        }
        let mut end = None;
        for e in (1..=n).filter(|&e| skip(input, e) == n) {
            if let Some(step) = &best[e]
                && end.is_none_or(|end: usize| beats(step, best[end].as_ref().unwrap()))
            {
                end = Some(e);
            }
        }
        let end = end?;
        let lens = &best[end].as_ref()?.lens;
        let (words, long) = (lens.len(), lens.iter().any(|&l| l > 1));
        let mut parts = Vec::new();
        let mut at = end;
        while at > 0 {
            let step = best[at].as_ref()?;
            parts.push(step.text.as_str());
            at = step.from;
        }
        parts.reverse();
        Some((parts.concat(), words, long))
    }

    /// Candidates for `input` (lowercase a-z and '), best first; each
    /// covers the first `consumed` bytes of input.
    pub fn candidates(&self, input: &str, limit: usize) -> Vec<Candidate> {
        if !input.bytes().all(|b| b.is_ascii_alphabetic() || b == b'\'') {
            return Vec::new();
        }
        let input = input.to_ascii_lowercase();
        let bytes = input.as_bytes();
        let lattice = self.lattice(&input);
        // (the sentence, then the rest, then rare characters; more input
        // covered, fewer abbreviations, chosen more often, top characters
        // before phrases before the other characters, fewer syllables, rank)
        type Order = (u8, Reverse<usize>, usize, Reverse<u32>, u8, usize, u32);
        let mut all: Vec<(Order, String, usize)> = Vec::new();
        if let Some((text, words, long)) = self.sentence(bytes, &lattice)
            && words > 1
            && long
        {
            let order = (0, Reverse(0), 0, Reverse(0), 0, 0, 0);
            all.push((order, text, bytes.len()));
        }
        let start = skip(bytes, 0);
        let mut found = Vec::new();
        self.words(bytes, &lattice, start, &mut found);
        for w in found {
            let order = (
                1,
                Reverse(w.end),
                w.abbr,
                Reverse(w.user),
                1,
                w.tokens,
                w.rank,
            );
            all.push((order, w.text, w.end));
        }
        for t in lattice.get(start).map_or(&[][..], |v| &v[..]) {
            for (n, w) in self.singles(t).into_iter().enumerate() {
                let class = if n < 3 { 0 } else { 2 };
                let rare = w.user == 0 && w.rank >> (ORDER_BITS + 1) >= 2;
                let band = if rare { 2 } else { 1 };
                let order = (
                    band,
                    Reverse(w.end),
                    w.abbr,
                    Reverse(w.user),
                    class,
                    1,
                    w.rank,
                );
                all.push((order, w.text, w.end));
            }
        }
        all.sort_by_key(|a| a.0);
        let mut seen = HashSet::new();
        all.into_iter()
            .filter(|(_, text, _)| seen.insert(text.clone()))
            .take(limit)
            .map(|(_, text, consumed)| Candidate { text, consumed })
            .collect()
    }

    /// The initials `text` can be typed with (several for characters with
    /// several readings), at most 16; none when a character has no reading.
    fn user_key(&self, text: &str) -> Vec<Vec<u8>> {
        let mut keys = vec![Vec::new()];
        for c in text.chars() {
            let mut letters: Vec<u8> = self
                .readings(c)
                .iter()
                .map(|&s| self.names[s as usize].as_bytes()[0])
                .collect();
            letters.sort_unstable();
            letters.dedup();
            if letters.is_empty() {
                return Vec::new();
            }
            keys = keys
                .iter()
                .flat_map(|k| {
                    letters.iter().map(move |&l| {
                        let mut k = k.clone();
                        k.push(l);
                        k
                    })
                })
                .take(16)
                .collect();
        }
        keys
    }

    fn index_word(&mut self, word: &str) {
        let shared: Arc<str> = Arc::from(word);
        for key in self.user_key(word) {
            let words = self.user_keys.entry(key).or_default();
            if !words.iter().any(|w| **w == *word) {
                words.push(shared.clone());
            }
        }
    }

    fn index_user(&mut self) {
        self.user_keys.clear();
        let words: Vec<String> = self.user.words.keys().cloned().collect();
        for word in words {
            self.index_word(&word);
        }
    }

    /// The user chose `text` after `previous` (the last committed text, if
    /// any): learn it.
    pub fn learn(&mut self, previous: Option<&str>, text: &str) {
        let new = self.user.count(text) == 0;
        if self.user.learn(previous, text) {
            self.index_user();
        } else if new {
            self.index_word(text);
        }
    }

    /// Words that followed `previous` before (association), best first.
    pub fn follow(&self, previous: &str, limit: usize) -> Vec<String> {
        self.user.follow(previous, limit)
    }

    /// Writes the user dictionary when it changed.
    pub fn save(&mut self) -> Result<()> {
        self.user.save()
    }
}
