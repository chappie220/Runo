//! Keys from evdev: a US keymap with Shift, CapsLock, Ctrl and Alt, and the
//! bytes xterm sends for each key.

/// `input_event` on this machine: a timeval, then type, code and value.
pub const EVENT_SIZE: usize = 2 * std::mem::size_of::<libc::c_long>() + 8;
pub const EV_SYN: u16 = 0;
pub const EV_KEY: u16 = 1;
pub const EV_LED: u16 = 0x11;
pub const LED_CAPSL: u16 = 1;

pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_RIGHTSHIFT: u16 = 54;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_SPACE: u16 = 57;
pub const KEY_CAPSLOCK: u16 = 58;
pub const KEY_RIGHTCTRL: u16 = 97;
pub const KEY_RIGHTALT: u16 = 100;
pub const KEY_A: u16 = 30;
pub const KEY_Z: u16 = 44;

/// (type, code, value) of each event in `bytes`.
pub fn parse_events(bytes: &[u8]) -> Vec<(u16, u16, i32)> {
    let at = EVENT_SIZE - 8;
    bytes
        .as_chunks::<EVENT_SIZE>()
        .0
        .iter()
        .map(|e| {
            let kind = u16::from_ne_bytes([e[at], e[at + 1]]);
            let code = u16::from_ne_bytes([e[at + 2], e[at + 3]]);
            let value = i32::from_ne_bytes([e[at + 4], e[at + 5], e[at + 6], e[at + 7]]);
            (kind, code, value)
        })
        .collect()
}

/// One event as the kernel reads it, with a zero time.
pub fn event_bytes(kind: u16, code: u16, value: i32) -> Vec<u8> {
    let mut out = vec![0u8; EVENT_SIZE - 8];
    out.extend_from_slice(&kind.to_ne_bytes());
    out.extend_from_slice(&code.to_ne_bytes());
    out.extend_from_slice(&value.to_ne_bytes());
    out
}

/// A key other than a character.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sym {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
}

/// A key pressed (or repeated), with the modifiers held. A character has
/// Shift and CapsLock applied already.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Press {
    pub sym: Sym,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Press(Press),
    /// Shift went down and up with no other key.
    ShiftTap,
    /// CapsLock changed: on or off, for the light.
    Caps(bool),
}

/// The characters of a key without and with Shift.
fn chars(code: u16) -> Option<(char, char)> {
    const ROWS: [(u16, &str, &str); 4] = [
        (2, "1234567890-=", "!@#$%^&*()_+"),
        (16, "qwertyuiop[]", "QWERTYUIOP{}"),
        (30, "asdfghjkl;'`", "ASDFGHJKL:\"~"),
        (43, "\\zxcvbnm,./", "|ZXCVBNM<>?"),
    ];
    for (start, plain, shifted) in ROWS {
        let at = code.checked_sub(start)? as usize;
        if let (Some(p), Some(s)) = (plain.chars().nth(at), shifted.chars().nth(at)) {
            return Some((p, s));
        }
    }
    match code {
        KEY_SPACE => Some((' ', ' ')),
        // The extra key of ISO keyboards.
        86 => Some(('\\', '|')),
        55 => Some(('*', '*')),
        74 => Some(('-', '-')),
        78 => Some(('+', '+')),
        98 => Some(('/', '/')),
        117 => Some(('=', '=')),
        83 => Some(('.', '.')),
        71..=73 | 75..=77 | 79..=82 => {
            let digit = match code {
                71..=73 => b'7' + (code - 71) as u8,
                75..=77 => b'4' + (code - 75) as u8,
                79..=81 => b'1' + (code - 79) as u8,
                _ => b'0',
            };
            Some((digit as char, digit as char))
        }
        _ => None,
    }
}

fn special(code: u16) -> Option<Sym> {
    Some(match code {
        1 => Sym::Escape,
        14 => Sym::Backspace,
        15 => Sym::Tab,
        28 | 96 => Sym::Enter,
        59..=68 => Sym::F((code - 58) as u8),
        87 => Sym::F(11),
        88 => Sym::F(12),
        102 => Sym::Home,
        103 => Sym::Up,
        104 => Sym::PageUp,
        105 => Sym::Left,
        106 => Sym::Right,
        107 => Sym::End,
        108 => Sym::Down,
        109 => Sym::PageDown,
        110 => Sym::Insert,
        111 => Sym::Delete,
        _ => return None,
    })
}

/// Modifier state across every keyboard.
#[derive(Default)]
pub struct Keyboard {
    shift: [bool; 2],
    ctrl: [bool; 2],
    alt: [bool; 2],
    caps: bool,
    /// Shift is down and nothing else was pressed since.
    shift_alone: bool,
}

impl Keyboard {
    fn shift(&self) -> bool {
        self.shift[0] || self.shift[1]
    }

    /// Reads one EV_KEY event: `value` 1 press, 2 repeat, 0 release.
    pub fn key(&mut self, code: u16, value: i32) -> Option<Event> {
        let down = value != 0;
        let modifier = match code {
            KEY_LEFTSHIFT | KEY_RIGHTSHIFT => {
                let side = (code == KEY_RIGHTSHIFT) as usize;
                let others = self.ctrl.iter().chain(&self.alt).any(|&m| m);
                if value == 1 && !self.shift() {
                    self.shift_alone = !others;
                }
                self.shift[side] = down;
                if !down && !self.shift() && std::mem::take(&mut self.shift_alone) {
                    return Some(Event::ShiftTap);
                }
                return None;
            }
            KEY_LEFTCTRL | KEY_RIGHTCTRL => Some(&mut self.ctrl[(code == KEY_RIGHTCTRL) as usize]),
            KEY_LEFTALT | KEY_RIGHTALT => Some(&mut self.alt[(code == KEY_RIGHTALT) as usize]),
            _ => None,
        };
        if down {
            self.shift_alone = false;
        }
        if let Some(held) = modifier {
            *held = down;
            return None;
        }
        if !down {
            return None;
        }
        if code == KEY_CAPSLOCK {
            if value == 1 {
                self.caps = !self.caps;
                return Some(Event::Caps(self.caps));
            }
            return None;
        }
        let shift = self.shift();
        let sym = match chars(code) {
            Some((plain, shifted)) => {
                let upper = if plain.is_ascii_alphabetic() {
                    shift != self.caps
                } else {
                    shift
                };
                Sym::Char(if upper { shifted } else { plain })
            }
            None => special(code)?,
        };
        Some(Event::Press(Press {
            sym,
            shift,
            ctrl: self.ctrl[0] || self.ctrl[1],
            alt: self.alt[0] || self.alt[1],
        }))
    }
}

/// The control character Ctrl makes of `c`, if any.
fn control(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' | 'A'..='Z' => c.to_ascii_lowercase() as u8 & 0x1f,
        ' ' | '@' | '2' | '`' => 0,
        '[' | '{' | '3' => 0x1b,
        '\\' | '|' | '4' => 0x1c,
        ']' | '}' | '5' => 0x1d,
        '^' | '~' | '6' => 0x1e,
        '_' | '-' | '/' | '?' | '7' => 0x1f,
        '8' => 0x7f,
        _ => return None,
    })
}

/// What xterm sends for `press`; `app_cursor` is DECCKM.
pub fn encode(press: &Press, app_cursor: bool) -> Vec<u8> {
    // xterm's modifier parameter: 1 + Shift 1 + Alt 2 + Ctrl 4.
    let modifier = 1 + press.shift as u8 + 2 * press.alt as u8 + 4 * press.ctrl as u8;
    let mut out = Vec::new();
    let alt_prefix = |out: &mut Vec<u8>| {
        if press.alt {
            out.push(0x1b);
        }
    };
    // `CSI 1;m x`, or `SS3 x` / `CSI x` without modifiers.
    let letter = |x: u8, ss3: bool| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{}", x as char).into_bytes()
        } else if ss3 {
            vec![0x1b, b'O', x]
        } else {
            vec![0x1b, b'[', x]
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[{n};{modifier}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };
    match press.sym {
        Sym::Char(c) => {
            alt_prefix(&mut out);
            match control(c).filter(|_| press.ctrl) {
                Some(byte) => out.push(byte),
                None => {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            }
        }
        Sym::Enter => {
            alt_prefix(&mut out);
            out.push(b'\r');
        }
        Sym::Tab if press.shift => out.extend_from_slice(b"\x1b[Z"),
        Sym::Tab => {
            alt_prefix(&mut out);
            out.push(b'\t');
        }
        Sym::Backspace => {
            alt_prefix(&mut out);
            out.push(if press.ctrl { 0x08 } else { 0x7f });
        }
        Sym::Escape => {
            alt_prefix(&mut out);
            out.push(0x1b);
        }
        Sym::Up => out = letter(b'A', app_cursor),
        Sym::Down => out = letter(b'B', app_cursor),
        Sym::Right => out = letter(b'C', app_cursor),
        Sym::Left => out = letter(b'D', app_cursor),
        Sym::Home => out = letter(b'H', app_cursor),
        Sym::End => out = letter(b'F', app_cursor),
        Sym::Insert => out = tilde(2),
        Sym::Delete => out = tilde(3),
        Sym::PageUp => out = tilde(5),
        Sym::PageDown => out = tilde(6),
        Sym::F(n @ 1..=4) => out = letter(b'P' + n - 1, true),
        Sym::F(n) => {
            const CODES: [u8; 8] = [15, 17, 18, 19, 20, 21, 23, 24];
            if let Some(&code) = CODES.get(n as usize - 5) {
                out = tilde(code);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type_keys(keyboard: &mut Keyboard, keys: &[(u16, i32)]) -> Vec<Event> {
        keys.iter()
            .filter_map(|&(code, value)| keyboard.key(code, value))
            .collect()
    }

    fn sym(event: &Event) -> Sym {
        match event {
            Event::Press(p) => p.sym,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn maps_the_us_layout() {
        let mut k = Keyboard::default();
        let events = type_keys(
            &mut k,
            &[
                (KEY_A, 1),
                (KEY_A, 0),
                (2, 1),
                (12, 1),
                (43, 1),
                (52, 1),
                (KEY_SPACE, 1),
            ],
        );
        let syms: Vec<Sym> = events.iter().map(sym).collect();
        assert_eq!(
            syms,
            ['a', '1', '-', '\\', '.', ' '].map(Sym::Char).to_vec()
        );
        let events = type_keys(
            &mut k,
            &[
                (KEY_LEFTSHIFT, 1),
                (KEY_A, 1),
                (2, 1),
                (40, 1),
                (53, 1),
                (KEY_LEFTSHIFT, 0),
            ],
        );
        let syms: Vec<Sym> = events.iter().map(sym).collect();
        assert_eq!(syms, ['A', '!', '"', '?'].map(Sym::Char).to_vec());
        // CapsLock changes letters only, and Shift undoes it.
        let events = type_keys(&mut k, &[(KEY_CAPSLOCK, 1), (KEY_CAPSLOCK, 0)]);
        assert_eq!(events, [Event::Caps(true)]);
        let events = type_keys(
            &mut k,
            &[
                (KEY_Z, 1),
                (3, 1),
                (KEY_RIGHTSHIFT, 1),
                (KEY_Z, 1),
                (KEY_RIGHTSHIFT, 0),
            ],
        );
        let syms: Vec<Sym> = events.iter().map(sym).collect();
        assert_eq!(syms, ['Z', '2', 'z'].map(Sym::Char).to_vec());
        // Repeats are presses; releases are not.
        let events = type_keys(&mut k, &[(103, 1), (103, 2), (103, 2), (103, 0)]);
        assert_eq!(events.len(), 3);
        assert_eq!(sym(&events[0]), Sym::Up);
        // Keypad keys type their characters.
        let events = type_keys(&mut k, &[(79, 1), (82, 1), (96, 1)]);
        let syms: Vec<Sym> = events.iter().map(sym).collect();
        assert_eq!(syms, [Sym::Char('1'), Sym::Char('0'), Sym::Enter]);
    }

    #[test]
    fn shift_alone_is_a_tap() {
        let mut k = Keyboard::default();
        assert_eq!(
            type_keys(
                &mut k,
                &[(KEY_LEFTSHIFT, 1), (KEY_LEFTSHIFT, 2), (KEY_LEFTSHIFT, 0)]
            ),
            [Event::ShiftTap]
        );
        // Not when it typed something, nor with Ctrl.
        let events = type_keys(
            &mut k,
            &[(KEY_LEFTSHIFT, 1), (KEY_A, 1), (KEY_LEFTSHIFT, 0)],
        );
        assert!(!events.contains(&Event::ShiftTap));
        let events = type_keys(
            &mut k,
            &[
                (KEY_LEFTCTRL, 1),
                (KEY_LEFTSHIFT, 1),
                (KEY_LEFTSHIFT, 0),
                (KEY_LEFTCTRL, 0),
            ],
        );
        assert!(events.is_empty());
    }

    #[test]
    fn encodes_like_xterm() {
        let press = |sym, shift, ctrl, alt| Press {
            sym,
            shift,
            ctrl,
            alt,
        };
        let enc = |p: Press| encode(&p, false);
        assert_eq!(enc(press(Sym::Char('a'), false, false, false)), b"a");
        assert_eq!(enc(press(Sym::Char('c'), false, true, false)), [3]);
        assert_eq!(enc(press(Sym::Char('C'), true, true, false)), [3]);
        assert_eq!(enc(press(Sym::Char(' '), false, true, false)), [0]);
        assert_eq!(enc(press(Sym::Char('['), false, true, false)), [0x1b]);
        assert_eq!(enc(press(Sym::Char('x'), false, false, true)), b"\x1bx");
        assert_eq!(
            enc(press(Sym::Char('中'), false, false, false)),
            "中".as_bytes()
        );
        assert_eq!(enc(press(Sym::Enter, false, false, false)), b"\r");
        assert_eq!(enc(press(Sym::Backspace, false, false, false)), [0x7f]);
        assert_eq!(enc(press(Sym::Backspace, false, false, true)), b"\x1b\x7f");
        assert_eq!(enc(press(Sym::Escape, false, false, false)), [0x1b]);
        assert_eq!(enc(press(Sym::Tab, false, false, false)), b"\t");
        assert_eq!(enc(press(Sym::Tab, true, false, false)), b"\x1b[Z");
        assert_eq!(enc(press(Sym::Up, false, false, false)), b"\x1b[A");
        assert_eq!(
            encode(&press(Sym::Up, false, false, false), true),
            b"\x1bOA"
        );
        assert_eq!(enc(press(Sym::Left, false, true, false)), b"\x1b[1;5D");
        assert_eq!(enc(press(Sym::Right, true, false, true)), b"\x1b[1;4C");
        assert_eq!(enc(press(Sym::Home, false, false, false)), b"\x1b[H");
        assert_eq!(enc(press(Sym::End, false, false, false)), b"\x1b[F");
        assert_eq!(enc(press(Sym::PageUp, false, false, false)), b"\x1b[5~");
        assert_eq!(enc(press(Sym::PageDown, false, true, false)), b"\x1b[6;5~");
        assert_eq!(enc(press(Sym::Insert, false, false, false)), b"\x1b[2~");
        assert_eq!(enc(press(Sym::Delete, false, false, false)), b"\x1b[3~");
        assert_eq!(enc(press(Sym::F(1), false, false, false)), b"\x1bOP");
        assert_eq!(enc(press(Sym::F(4), true, false, false)), b"\x1b[1;2S");
        assert_eq!(enc(press(Sym::F(5), false, false, false)), b"\x1b[15~");
        assert_eq!(enc(press(Sym::F(12), false, false, false)), b"\x1b[24~");
    }

    #[test]
    fn reads_input_events() {
        let mut bytes = event_bytes(EV_KEY, KEY_A, 1);
        bytes.extend(event_bytes(EV_SYN, 0, 0));
        bytes.extend(event_bytes(EV_KEY, KEY_A, 0));
        assert_eq!(bytes.len(), 3 * EVENT_SIZE);
        assert_eq!(
            parse_events(&bytes),
            [(EV_KEY, KEY_A, 1), (EV_SYN, 0, 0), (EV_KEY, KEY_A, 0)]
        );
        if cfg!(target_pointer_width = "64") {
            assert_eq!(EVENT_SIZE, 24);
        }
    }
}
