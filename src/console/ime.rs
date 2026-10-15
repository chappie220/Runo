//! Chinese input in the console: the seam to an input method, Chinese
//! punctuation, and the bar at the bottom of the screen that shows the mode,
//! what is being typed and the candidates.

use crate::i18n::{Lang, Tr, tr};

use super::keys::{self, Press, Sym};
use super::term::{self, Cell, Color, Pen};

/// A key as an input method sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// What an input method did with a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Not its key: it goes to the program as usual.
    Pass,
    /// Taken; what is shown may have changed.
    Consumed,
    /// Text to send to the program.
    Commit(String),
}

pub trait InputMethod {
    fn key(&mut self, key: Key) -> Outcome;
    /// What is being typed, e.g. `ni hao`.
    fn preedit(&self) -> String;
    /// The candidates shown and which one is highlighted.
    fn page(&self) -> (Vec<String>, usize);
    /// Nothing is being typed.
    fn is_empty(&self) -> bool;
    /// Keeps what it learnt (the user's words).
    fn save(&mut self) -> anyhow::Result<()>;
}

const HINT: Tr = tr("Ctrl+Space or Shift: Chinese", "Ctrl+空格或 Shift：中/英");

#[cfg(test)]
pub const ALL: &[Tr] = &[HINT];

/// The bar's colours.
const BAR: Pen = Pen {
    fg: Color::Rgb(0xc8, 0xcc, 0xd2),
    bg: Color::Rgb(0x26, 0x2a, 0x31),
    flags: 0,
};
const BADGE_ZH: Pen = Pen {
    fg: Color::Rgb(0xff, 0xff, 0xff),
    bg: Color::Rgb(0xc0, 0x39, 0x2b),
    flags: term::BOLD,
};
const BADGE_EN: Pen = Pen {
    fg: Color::Rgb(0x26, 0x2a, 0x31),
    bg: Color::Rgb(0x9a, 0xa0, 0xa8),
    flags: term::BOLD,
};
const CHOSEN: Pen = Pen {
    fg: Color::Rgb(0x26, 0x2a, 0x31),
    bg: Color::Rgb(0x8c, 0xd1, 0x8c),
    flags: 0,
};
const DIM: Pen = Pen {
    fg: Color::Rgb(0x80, 0x86, 0x8e),
    ..BAR
};

/// Typing in the console: English, or Chinese through the input method.
pub struct Typing {
    ime: Option<Box<dyn InputMethod>>,
    chinese: bool,
    /// The next `"` and `'` open a quote.
    double_open: bool,
    single_open: bool,
    /// The last character typed was a digit: `.` stays a decimal point.
    after_digit: bool,
    /// Something was committed since the last save.
    unsaved: bool,
}

impl Typing {
    pub fn new(ime: Option<Box<dyn InputMethod>>) -> Self {
        Typing {
            ime,
            chinese: false,
            double_open: true,
            single_open: true,
            after_digit: false,
            unsaved: false,
        }
    }

    #[cfg(test)]
    pub fn chinese(&self) -> bool {
        self.chinese
    }

    /// Switches between English and Chinese; returns what to send. Letters
    /// typed but not chosen go out as they are.
    pub fn toggle(&mut self) -> Vec<u8> {
        let Some(ime) = self.ime.as_mut() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if self.chinese && !ime.is_empty() {
            let typed: String = ime
                .preedit()
                .chars()
                .filter(|c| !matches!(c, ' ' | '\''))
                .collect();
            ime.key(Key::Escape);
            out = typed.into_bytes();
        }
        self.chinese = !self.chinese;
        out
    }

    /// What a key sends to the program.
    pub fn press(&mut self, press: &Press, app_cursor: bool) -> Vec<u8> {
        if press.ctrl && !press.alt && press.sym == Sym::Char(' ') && self.ime.is_some() {
            return self.toggle();
        }
        if self.chinese
            && !press.ctrl
            && !press.alt
            && let Some(key) = ime_key(press.sym)
            && let Some(ime) = self.ime.as_mut()
        {
            match ime.key(key) {
                Outcome::Pass => {}
                Outcome::Consumed => return Vec::new(),
                Outcome::Commit(text) => {
                    self.unsaved = true;
                    self.after_digit = false;
                    return text.into_bytes();
                }
            }
            if let Sym::Char(c) = press.sym {
                let out = self.punctuation(c);
                self.after_digit = c.is_ascii_digit();
                return out.into_bytes();
            }
        }
        if let Sym::Char(c) = press.sym {
            self.after_digit = c.is_ascii_digit();
        }
        keys::encode(press, app_cursor)
    }

    /// `c` as Chinese punctuation, when it has a Chinese form.
    fn punctuation(&mut self, c: char) -> String {
        let quote = |open: &mut bool, pair: [&str; 2]| {
            let out = pair[!*open as usize];
            *open = !*open;
            out.to_owned()
        };
        match c {
            '.' if self.after_digit => ".".into(),
            ',' if self.after_digit => ",".into(),
            '"' => quote(&mut self.double_open, ["“", "”"]),
            '\'' => quote(&mut self.single_open, ["‘", "’"]),
            _ => match "，。？！：；（）《》、…—￥【】～·"
                .chars()
                .zip(",.?!:;()<>\\^_$[]~`".chars())
                .find(|&(_, ascii)| ascii == c)
            {
                Some(('…', _)) => "……".into(),
                Some(('—', _)) => "——".into(),
                Some((zh, _)) => zh.to_string(),
                None => c.to_string(),
            },
        }
    }

    /// Saves the input method's words if it learnt any.
    pub fn save(&mut self) {
        if let Some(ime) = self.ime.as_mut()
            && std::mem::take(&mut self.unsaved)
            && let Err(err) = ime.save()
        {
            eprintln!("console: saving the input method's words: {err:#}");
        }
    }

    /// The bar: mode, what is typed, the candidates, or `note`.
    pub fn bar(&self, cols: usize, lang: Lang, note: Option<&str>) -> Vec<Cell> {
        let mut line = Vec::new();
        let (badge, pen) = if self.chinese {
            (" 中 ", BADGE_ZH)
        } else {
            (" EN ", BADGE_EN)
        };
        line.extend(term::cells(badge, pen));
        line.extend(term::cells(" ", BAR));
        if let Some(note) = note {
            line.extend(term::cells(note, BAR));
        } else if let Some(ime) = self
            .ime
            .as_ref()
            .filter(|ime| self.chinese && !ime.is_empty())
        {
            line.extend(term::cells(
                &ime.preedit(),
                Pen {
                    flags: term::UNDERLINE,
                    ..BAR
                },
            ));
            line.extend(term::cells("  ", BAR));
            let (candidates, chosen) = ime.page();
            for (i, candidate) in candidates.iter().enumerate() {
                let number = term::cells(&format!("{}.", (i + 1) % 10), DIM);
                let pen = if i == chosen { CHOSEN } else { BAR };
                let word = term::cells(candidate, pen);
                if line.len() + number.len() + word.len() > cols {
                    break;
                }
                line.extend(number);
                line.extend(word);
                line.extend(term::cells(" ", BAR));
            }
        } else if self.ime.is_some() {
            line.extend(term::cells(HINT.get(lang), DIM));
        }
        line.truncate(cols);
        // A wide character cut in half.
        if line.last().is_some_and(|c| c.flags & term::WIDE != 0) {
            line.pop();
        }
        line.resize(cols, Cell { ch: ' ', ..blank() });
        line
    }
}

fn blank() -> Cell {
    Cell {
        fg: BAR.fg,
        bg: BAR.bg,
        ..Cell::default()
    }
}

fn ime_key(sym: Sym) -> Option<Key> {
    Some(match sym {
        Sym::Char(' ') => Key::Space,
        Sym::Char(c @ '0'..='9') => Key::Digit(c as u8 - b'0'),
        Sym::Char(c) => Key::Char(c),
        Sym::Backspace => Key::Backspace,
        Sym::Enter => Key::Enter,
        Sym::Escape => Key::Escape,
        Sym::Left => Key::Left,
        Sym::Right => Key::Right,
        Sym::Up => Key::Up,
        Sym::Down => Key::Down,
        Sym::PageUp => Key::PageUp,
        Sym::PageDown => Key::PageDown,
        _ => return None,
    })
}

/// The built-in pinyin input method, with the user's words in `user`; if
/// they cannot be read or kept, without them (and it says so).
pub fn pinyin(user: Option<std::path::PathBuf>) -> anyhow::Result<Box<dyn InputMethod>> {
    let engine = match crate::pinyin::Engine::load(user) {
        Ok(engine) => engine,
        Err(err) => {
            eprintln!("pinyin: {err:#}; the words you type are not kept");
            crate::pinyin::Engine::load(None)?
        }
    };
    Ok(Box::new(crate::pinyin::Composer::new(engine)))
}

impl InputMethod for crate::pinyin::Composer {
    fn key(&mut self, key: Key) -> Outcome {
        use crate::pinyin::{Key as K, Outcome as O};
        let key = match key {
            Key::Char(c) => K::Char(c),
            Key::Backspace => K::Backspace,
            Key::Enter => K::Enter,
            Key::Space => K::Space,
            Key::Escape => K::Escape,
            Key::Left => K::Left,
            Key::Right => K::Right,
            Key::Up => K::Up,
            Key::Down => K::Down,
            Key::PageUp => K::PageUp,
            Key::PageDown => K::PageDown,
            Key::Digit(d) => K::Digit(d),
        };
        match crate::pinyin::Composer::key(self, key) {
            O::Pass => Outcome::Pass,
            O::Consumed => Outcome::Consumed,
            O::Commit(text) => Outcome::Commit(text),
        }
    }

    fn preedit(&self) -> String {
        crate::pinyin::Composer::preedit(self)
    }

    fn page(&self) -> (Vec<String>, usize) {
        let (words, chosen) = crate::pinyin::Composer::page(self);
        (words.to_vec(), chosen)
    }

    fn is_empty(&self) -> bool {
        crate::pinyin::Composer::is_empty(self)
    }

    fn save(&mut self) -> anyhow::Result<()> {
        crate::pinyin::Composer::save(self)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Knows two words: `ni` and `zhong`; space or a digit picks.
    #[derive(Default)]
    pub struct Fake {
        typed: String,
        chosen: usize,
        pub saves: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl Fake {
        fn words(&self) -> Vec<String> {
            let words: &[&str] = match self.typed.as_str() {
                "ni" => &["你", "尼", "泥"],
                "zhong" => &["中", "种", "重"],
                _ => &[],
            };
            words.iter().map(|w| w.to_string()).collect()
        }
    }

    impl InputMethod for Fake {
        fn key(&mut self, key: Key) -> Outcome {
            match key {
                Key::Char(c) if c.is_ascii_lowercase() => {
                    self.typed.push(c);
                    Outcome::Consumed
                }
                _ if self.typed.is_empty() => Outcome::Pass,
                Key::Backspace => {
                    self.typed.pop();
                    Outcome::Consumed
                }
                Key::Escape => {
                    self.typed.clear();
                    Outcome::Consumed
                }
                Key::Right => {
                    self.chosen += 1;
                    Outcome::Consumed
                }
                Key::Space | Key::Digit(_) => {
                    let n = match key {
                        Key::Digit(d) => d as usize - 1,
                        _ => self.chosen,
                    };
                    let word = self.words().get(n).cloned().unwrap_or_default();
                    self.typed.clear();
                    self.chosen = 0;
                    Outcome::Commit(word)
                }
                _ => Outcome::Consumed,
            }
        }

        fn preedit(&self) -> String {
            self.typed.clone()
        }

        fn page(&self) -> (Vec<String>, usize) {
            (self.words(), self.chosen)
        }

        fn is_empty(&self) -> bool {
            self.typed.is_empty()
        }

        fn save(&mut self) -> anyhow::Result<()> {
            self.saves.set(self.saves.get() + 1);
            Ok(())
        }
    }

    pub fn press(sym: Sym) -> Press {
        Press {
            sym,
            shift: false,
            ctrl: false,
            alt: false,
        }
    }

    fn type_text(typing: &mut Typing, text: &str) -> String {
        let mut out = Vec::new();
        for c in text.chars() {
            out.extend(typing.press(&press(Sym::Char(c)), false));
        }
        String::from_utf8(out).unwrap()
    }

    fn bar_text(typing: &Typing, lang: Lang) -> String {
        typing
            .bar(60, lang, None)
            .iter()
            .filter(|c| c.flags & term::TAIL == 0)
            .map(|c| c.ch)
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    #[test]
    fn texts_are_translated() {
        crate::i18n::assert_complete(ALL);
    }

    #[test]
    fn types_chinese_through_the_input_method() {
        let fake = Fake::default();
        let saves = fake.saves.clone();
        let mut typing = Typing::new(Some(Box::new(fake)));
        assert_eq!(type_text(&mut typing, "ni"), "ni");
        let ctrl_space = Press {
            ctrl: true,
            ..press(Sym::Char(' '))
        };
        assert!(typing.press(&ctrl_space, false).is_empty());
        assert!(typing.chinese());
        assert_eq!(type_text(&mut typing, "ni"), "");
        assert_eq!(bar_text(&typing, Lang::En), " 中  ni  1.你 2.尼 3.泥");
        assert_eq!(type_text(&mut typing, " "), "你");
        assert_eq!(type_text(&mut typing, "zhong2"), "种");
        type_text(&mut typing, "ni");
        typing.press(&press(Sym::Right), false);
        let bar = typing.bar(60, Lang::En, None);
        let chosen: String = bar
            .iter()
            .filter(|c| c.bg == CHOSEN.bg && c.flags & term::TAIL == 0)
            .map(|c| c.ch)
            .collect();
        assert_eq!(chosen, "尼");
        // Arrows the input method does not want reach the program.
        typing.press(&press(Sym::Escape), false);
        assert_eq!(typing.press(&press(Sym::Up), false), b"\x1b[A");
        // Saved once for what was committed.
        typing.save();
        typing.save();
        assert_eq!(saves.get(), 1);
    }

    #[test]
    fn chinese_punctuation() {
        let mut typing = Typing::new(Some(Box::new(Fake::default())));
        typing.toggle();
        assert_eq!(type_text(&mut typing, ",.?!"), "，。？！");
        assert_eq!(type_text(&mut typing, "\"\""), "“”");
        assert_eq!(type_text(&mut typing, "''<>\\^_"), "‘’《》、……——");
        // After a digit `.` stays a decimal point.
        assert_eq!(type_text(&mut typing, "3.14."), "3.14.");
        assert_eq!(type_text(&mut typing, "x."), "");
        typing.press(&press(Sym::Escape), false);
        assert_eq!(type_text(&mut typing, "#"), "#");
        // Ctrl keys are not the input method's.
        let ctrl_c = Press {
            ctrl: true,
            ..press(Sym::Char('c'))
        };
        assert_eq!(typing.press(&ctrl_c, false), [3]);
    }

    #[test]
    fn leaving_chinese_sends_the_letters_typed() {
        let mut typing = Typing::new(Some(Box::new(Fake::default())));
        typing.toggle();
        type_text(&mut typing, "zho");
        assert_eq!(typing.toggle(), b"zho");
        assert!(!typing.chinese());
        assert_eq!(type_text(&mut typing, "a,"), "a,");
        assert_eq!(bar_text(&typing, Lang::Zh), " EN  Ctrl+空格或 Shift：中/英");
    }

    #[test]
    fn without_an_input_method_ctrl_space_is_sent() {
        let mut typing = Typing::new(None);
        let ctrl_space = Press {
            ctrl: true,
            ..press(Sym::Char(' '))
        };
        assert_eq!(typing.press(&ctrl_space, false), [0]);
        assert!(typing.toggle().is_empty());
        assert!(!typing.chinese());
        assert_eq!(bar_text(&typing, Lang::En), " EN");
        let note = typing.bar(20, Lang::En, Some("ended"));
        assert_eq!(note.len(), 20);
        assert_eq!(note[5].ch, 'e');
    }

    #[test]
    fn the_pinyin_input_method_types_chinese() {
        let dir = tempfile::tempdir().unwrap();
        let mut typing = Typing::new(Some(pinyin(Some(dir.path().join("words.tsv"))).unwrap()));
        typing.toggle();
        assert!(typing.chinese());
        assert_eq!(type_text(&mut typing, "nihao"), "");
        assert_eq!(
            typing.press(&press(Sym::Char(' ')), false),
            "你好".as_bytes()
        );
    }
}
