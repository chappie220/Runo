//! The key-by-key state machine a terminal drives: letters build up the
//! pinyin, Space or a digit chooses, and a whole input is committed once
//! every part of it was chosen.

use super::{Candidate, Engine};

/// Candidates shown at once.
const PAGE: usize = 9;
/// Candidates asked for per input.
const LIMIT: usize = 300;
/// Association candidates asked for after a commit.
const FOLLOW: usize = 27;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Backspace,
    Enter,
    Space,
    Escape,
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Digit(u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Not for the composer (it is empty and the key is not a letter):
    /// handle it normally.
    Pass,
    /// Taken; redraw the preedit and candidates.
    Consumed,
    /// Text to send to the program, e.g. a chosen word, or the raw letters
    /// on Enter.
    Commit(String),
}

pub struct Composer {
    engine: Engine,
    /// Letters not chosen yet.
    input: String,
    /// What was chosen so far, with the letters (and apostrophes) it took.
    chosen: Vec<(String, String)>,
    /// Candidates for `input`.
    list: Vec<Candidate>,
    /// Their texts, or the association words after a commit.
    texts: Vec<String>,
    /// Whether `texts` are association words.
    follow: bool,
    page: usize,
    cursor: usize,
    /// The last committed word, for learning pairs and association.
    last: Option<String>,
}

impl Composer {
    pub fn new(engine: Engine) -> Self {
        Composer {
            engine,
            input: String::new(),
            chosen: Vec::new(),
            list: Vec::new(),
            texts: Vec::new(),
            follow: false,
            page: 0,
            cursor: 0,
            last: None,
        }
    }

    fn composing(&self) -> bool {
        !self.input.is_empty() || !self.chosen.is_empty()
    }

    pub fn key(&mut self, key: Key) -> Outcome {
        if self.follow {
            match key {
                Key::Digit(d) if (1..=PAGE as u8).contains(&d) => {
                    let i = self.page * PAGE + usize::from(d) - 1;
                    if i < self.texts.len() {
                        return self.choose(i);
                    }
                    self.clear();
                }
                Key::Space => return self.choose(self.page * PAGE + self.cursor),
                Key::PageUp | Key::PageDown => {
                    self.turn(key == Key::PageDown);
                    return Outcome::Consumed;
                }
                Key::Escape => {
                    self.clear();
                    return Outcome::Consumed;
                }
                _ => self.clear(),
            }
        }
        if !self.composing() {
            return match key {
                Key::Char(c @ 'a'..='z') => {
                    self.input.push(c);
                    self.refresh();
                    Outcome::Consumed
                }
                _ => Outcome::Pass,
            };
        }
        match key {
            Key::Char(c @ ('a'..='z' | '\'')) => {
                self.input.push(c);
                self.refresh();
                Outcome::Consumed
            }
            Key::Char(c) => {
                let mut text = self.raw();
                text.push(c);
                self.clear();
                self.last = None;
                Outcome::Commit(text)
            }
            Key::Enter => {
                let text = self.raw();
                self.clear();
                self.last = None;
                Outcome::Commit(text)
            }
            Key::Space if self.list.is_empty() => self.key(Key::Enter),
            Key::Space => self.choose(self.page * PAGE + self.cursor),
            Key::Digit(d) => {
                let i = self.page * PAGE + usize::from(d).wrapping_sub(1);
                if (1..=PAGE as u8).contains(&d) && i < self.list.len() {
                    self.choose(i)
                } else {
                    Outcome::Consumed
                }
            }
            Key::Escape => {
                self.clear();
                Outcome::Consumed
            }
            Key::Backspace => {
                if self.input.pop().is_none()
                    && let Some((_, letters)) = self.chosen.pop()
                {
                    self.input = letters;
                }
                self.refresh();
                Outcome::Consumed
            }
            Key::Left | Key::Up => {
                self.step(false);
                Outcome::Consumed
            }
            Key::Right | Key::Down => {
                self.step(true);
                Outcome::Consumed
            }
            Key::PageUp | Key::PageDown => {
                self.turn(key == Key::PageDown);
                Outcome::Consumed
            }
        }
    }

    /// What is typed, as it would be committed by Enter.
    fn raw(&self) -> String {
        let mut text: String = self.chosen.iter().map(|(t, _)| t.as_str()).collect();
        text.push_str(&self.input);
        text
    }

    fn clear(&mut self) {
        self.input.clear();
        self.chosen.clear();
        self.list.clear();
        self.texts.clear();
        self.follow = false;
        self.page = 0;
        self.cursor = 0;
    }

    fn refresh(&mut self) {
        self.list = self.engine.candidates(&self.input, LIMIT);
        self.texts = self.list.iter().map(|c| c.text.clone()).collect();
        self.follow = false;
        self.page = 0;
        self.cursor = 0;
    }

    fn pages(&self) -> usize {
        self.texts.len().div_ceil(PAGE).max(1)
    }

    fn step(&mut self, forward: bool) {
        let at = self.page * PAGE + self.cursor;
        let at = if forward {
            (at + 1).min(self.texts.len().saturating_sub(1))
        } else {
            at.saturating_sub(1)
        };
        self.page = at / PAGE;
        self.cursor = at % PAGE;
    }

    fn turn(&mut self, forward: bool) {
        if forward && self.page + 1 < self.pages() {
            self.page += 1;
        } else if !forward && self.page > 0 {
            self.page -= 1;
        } else {
            return;
        }
        self.cursor = 0;
    }

    /// Chooses candidate `i` (of all pages).
    fn choose(&mut self, i: usize) -> Outcome {
        if self.follow {
            let Some(word) = self.texts.get(i).cloned() else {
                return Outcome::Consumed;
            };
            self.engine.learn(self.last.as_deref(), &word);
            self.committed(word.clone());
            return Outcome::Commit(word);
        }
        let Some(c) = self.list.get(i).cloned() else {
            return Outcome::Consumed;
        };
        let rest = self.input[c.consumed..].trim_start_matches('\'');
        let taken = self.input.len() - rest.len();
        let letters = self.input[..taken].to_string();
        self.input = rest.to_string();
        self.chosen.push((c.text, letters));
        if !self.input.is_empty() {
            self.refresh();
            return Outcome::Consumed;
        }
        let parts: Vec<String> = self.chosen.drain(..).map(|(t, _)| t).collect();
        let mut previous = self.last.clone();
        for part in &parts {
            self.engine.learn(previous.as_deref(), part);
            previous = Some(part.clone());
        }
        let whole = parts.concat();
        if parts.len() > 1 {
            self.engine.learn(self.last.as_deref(), &whole);
        }
        self.committed(whole.clone());
        Outcome::Commit(whole)
    }

    /// After a commit: clears, and shows what followed `word` before.
    fn committed(&mut self, word: String) {
        self.clear();
        self.texts = self.engine.follow(&word, FOLLOW);
        self.follow = !self.texts.is_empty();
        self.last = Some(word);
    }

    /// The letters typed so far, with the part already chosen shown as
    /// Chinese (e.g. "中国 ren"), for drawing.
    pub fn preedit(&self) -> String {
        let chosen: String = self.chosen.iter().map(|(t, _)| t.as_str()).collect();
        match (chosen.is_empty(), self.input.is_empty()) {
            (true, _) => self.input.clone(),
            (false, true) => chosen,
            (false, false) => format!("{chosen} {}", self.input),
        }
    }

    /// The current page of candidates (at most 9) and which one is
    /// highlighted.
    pub fn page(&self) -> (&[String], usize) {
        let start = (self.page * PAGE).min(self.texts.len());
        let end = (start + PAGE).min(self.texts.len());
        (&self.texts[start..end], self.cursor)
    }

    /// Nothing typed and no association shown.
    pub fn is_empty(&self) -> bool {
        !self.composing() && self.texts.is_empty()
    }

    /// Saves the user dictionary (call on exit and every so often).
    pub fn save(&mut self) -> anyhow::Result<()> {
        self.engine.save()
    }

    #[cfg(test)]
    pub(super) fn engine(&self) -> &Engine {
        &self.engine
    }
}
