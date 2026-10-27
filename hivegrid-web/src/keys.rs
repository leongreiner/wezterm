//! termwiz `KeyCode`/`Modifiers` -> `Input.dispatchKeyEvent` parameters.
//!
//! WezTerm hands the pane the key *after* its own key-binding layer: a character key arrives
//! as `Char(c)` with the shift state already applied (`Shift+a` is `Char('A')`), special keys
//! as named variants. The table below assumes a US layout for `code` and the Windows virtual
//! key code, the way Puppeteer's keyboard does; the produced character (`key`, `text`) is
//! always exact, which is what pages observe in `keydown.key` and what text fields insert.
//!
//! Headless Chrome has no menu bar, so the editing shortcuts (Ctrl+A/C/X/V/Z/Y) do nothing on
//! their own; they carry the editing `commands` Blink understands.

use serde_json::{json, Value};
use termwiz::input::{KeyCode, Modifiers};

/// CDP modifier bits: Alt=1, Ctrl=2, Meta/Command=4, Shift=8.
pub const MOD_ALT: u8 = 1;
pub const MOD_CTRL: u8 = 2;
pub const MOD_META: u8 = 4;
pub const MOD_SHIFT: u8 = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdpKey {
    /// `KeyboardEvent.key`.
    pub key: String,
    /// `KeyboardEvent.code` (US layout); empty when the key has no physical position.
    pub code: &'static str,
    /// Windows virtual key code (0 = unknown).
    pub vk: u32,
    /// Text the key produces; `None` for keys that do not type (and for Ctrl/Alt/Super chords).
    pub text: Option<String>,
    /// 0 standard, 1 left, 2 right, 3 numpad.
    pub location: u8,
    pub modifiers: u8,
    /// Blink editing commands (`selectAll`, `copy`, ...).
    pub commands: &'static [&'static str],
}

impl CdpKey {
    /// The `Input.dispatchKeyEvent` parameters for a press (`down`) or release.
    pub fn params(&self, down: bool) -> Value {
        let ty = if !down {
            "keyUp"
        } else if self.text.is_some() {
            "keyDown"
        } else {
            "rawKeyDown"
        };
        let mut v = json!({
            "type": ty,
            "modifiers": self.modifiers,
            "key": self.key,
            "windowsVirtualKeyCode": self.vk,
            "nativeVirtualKeyCode": self.vk,
            "location": self.location,
            "isKeypad": self.location == 3,
        });
        if !self.code.is_empty() {
            v["code"] = json!(self.code);
        }
        if down {
            if let Some(t) = &self.text {
                v["text"] = json!(t);
                v["unmodifiedText"] = json!(t);
            }
            if !self.commands.is_empty() {
                v["commands"] = json!(self.commands);
            }
        }
        v
    }
}

/// termwiz modifiers -> CDP bitmask.
pub fn cdp_modifiers(m: Modifiers) -> u8 {
    let mut out = 0;
    if m.intersects(Modifiers::ALT | Modifiers::LEFT_ALT | Modifiers::RIGHT_ALT) {
        out |= MOD_ALT;
    }
    if m.intersects(Modifiers::CTRL | Modifiers::LEFT_CTRL | Modifiers::RIGHT_CTRL) {
        out |= MOD_CTRL;
    }
    if m.contains(Modifiers::SUPER) {
        out |= MOD_META;
    }
    if m.intersects(Modifiers::SHIFT | Modifiers::LEFT_SHIFT | Modifiers::RIGHT_SHIFT) {
        out |= MOD_SHIFT;
    }
    out
}

fn special(
    key: &str,
    code: &'static str,
    vk: u32,
    text: Option<&str>,
    location: u8,
    modifiers: u8,
    commands: &'static [&'static str],
) -> CdpKey {
    CdpKey {
        key: key.to_string(),
        code,
        vk,
        text: text.map(str::to_string),
        location,
        modifiers,
        commands,
    }
}

const FUNCTION_NAMES: [&str; 24] = [
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14", "F15",
    "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24",
];

const NUMPAD_CODES: [&str; 10] = [
    "Numpad0", "Numpad1", "Numpad2", "Numpad3", "Numpad4", "Numpad5", "Numpad6", "Numpad7",
    "Numpad8", "Numpad9",
];

/// Translate a key press or release. `None` for keys the page has no use for (bare modifiers,
/// media keys, unknown codes).
pub fn translate(key: &KeyCode, mods: Modifiers) -> Option<CdpKey> {
    let m = cdp_modifiers(mods);
    let ctrl = m & MOD_CTRL != 0;
    let shift = m & MOD_SHIFT != 0;
    let alt = m & MOD_ALT != 0;
    let k = |name: &str, code, vk| special(name, code, vk, None, 0, m, &[]);
    let pad = |name: &str, code, vk, text: Option<&str>| special(name, code, vk, text, 3, m, &[]);

    Some(match *key {
        KeyCode::Char(c) => return translate_char(c, m),

        KeyCode::Enter => special("Enter", "Enter", 13, Some("\r"), 0, m, &[]),
        KeyCode::Tab => k("Tab", "Tab", 9),
        KeyCode::Backspace => k("Backspace", "Backspace", 8),
        KeyCode::Escape => k("Escape", "Escape", 27),
        KeyCode::Delete => special(
            "Delete",
            "Delete",
            46,
            None,
            0,
            m,
            if shift && !ctrl && !alt { &["cut"] } else { &[] },
        ),
        KeyCode::Insert => special(
            "Insert",
            "Insert",
            45,
            None,
            0,
            m,
            if ctrl && !shift && !alt {
                &["copy"]
            } else if shift && !ctrl && !alt {
                &["paste"]
            } else {
                &[]
            },
        ),
        KeyCode::Home => k("Home", "Home", 36),
        KeyCode::End => k("End", "End", 35),
        KeyCode::PageUp => k("PageUp", "PageUp", 33),
        KeyCode::PageDown => k("PageDown", "PageDown", 34),
        KeyCode::LeftArrow | KeyCode::ApplicationLeftArrow => k("ArrowLeft", "ArrowLeft", 37),
        KeyCode::UpArrow | KeyCode::ApplicationUpArrow => k("ArrowUp", "ArrowUp", 38),
        KeyCode::RightArrow | KeyCode::ApplicationRightArrow => k("ArrowRight", "ArrowRight", 39),
        KeyCode::DownArrow | KeyCode::ApplicationDownArrow => k("ArrowDown", "ArrowDown", 40),
        KeyCode::KeyPadHome => pad("Home", "Numpad7", 36, None),
        KeyCode::KeyPadEnd => pad("End", "Numpad1", 35, None),
        KeyCode::KeyPadPageUp => pad("PageUp", "Numpad9", 33, None),
        KeyCode::KeyPadPageDown => pad("PageDown", "Numpad3", 34, None),
        KeyCode::KeyPadBegin => pad("Clear", "Numpad5", 12, None),

        KeyCode::Function(n) if (1..=24).contains(&n) => {
            let name = FUNCTION_NAMES[(n - 1) as usize];
            k(name, name, 111 + n as u32)
        }

        KeyCode::Numpad0
        | KeyCode::Numpad1
        | KeyCode::Numpad2
        | KeyCode::Numpad3
        | KeyCode::Numpad4
        | KeyCode::Numpad5
        | KeyCode::Numpad6
        | KeyCode::Numpad7
        | KeyCode::Numpad8
        | KeyCode::Numpad9 => {
            let n = match *key {
                KeyCode::Numpad0 => 0,
                KeyCode::Numpad1 => 1,
                KeyCode::Numpad2 => 2,
                KeyCode::Numpad3 => 3,
                KeyCode::Numpad4 => 4,
                KeyCode::Numpad5 => 5,
                KeyCode::Numpad6 => 6,
                KeyCode::Numpad7 => 7,
                KeyCode::Numpad8 => 8,
                KeyCode::Numpad9 => 9,
                _ => unreachable!(),
            };
            let digit = char::from(b'0' + n as u8).to_string();
            let text = (!(ctrl || alt || m & MOD_META != 0)).then_some(digit.as_str());
            pad(&digit, NUMPAD_CODES[n], 96 + n as u32, text)
        }
        KeyCode::Multiply => pad("*", "NumpadMultiply", 106, text_unless_chord(m, "*")),
        KeyCode::Add => pad("+", "NumpadAdd", 107, text_unless_chord(m, "+")),
        KeyCode::Separator => pad(",", "NumpadComma", 108, text_unless_chord(m, ",")),
        KeyCode::Subtract => pad("-", "NumpadSubtract", 109, text_unless_chord(m, "-")),
        KeyCode::Decimal => pad(".", "NumpadDecimal", 110, text_unless_chord(m, ".")),
        KeyCode::Divide => pad("/", "NumpadDivide", 111, text_unless_chord(m, "/")),

        KeyCode::Pause => k("Pause", "Pause", 19),
        KeyCode::CapsLock => k("CapsLock", "CapsLock", 20),
        KeyCode::NumLock => k("NumLock", "NumLock", 144),
        KeyCode::ScrollLock => k("ScrollLock", "ScrollLock", 145),
        KeyCode::Print | KeyCode::PrintScreen => k("PrintScreen", "PrintScreen", 44),
        KeyCode::Applications => k("ContextMenu", "ContextMenu", 93),
        KeyCode::Help => k("Help", "Help", 47),
        KeyCode::Select => k("Select", "", 41),
        KeyCode::Execute => k("Execute", "", 43),
        KeyCode::Clear => k("Clear", "", 12),
        KeyCode::Copy => special("Copy", "", 0, None, 0, m, &["copy"]),
        KeyCode::Cut => special("Cut", "", 0, None, 0, m, &["cut"]),
        KeyCode::Paste => special("Paste", "", 0, None, 0, m, &["paste"]),

        // Bare modifiers, media and browser keys, internal markers: nothing for the page.
        _ => return None,
    })
}

fn text_unless_chord(m: u8, t: &'static str) -> Option<&'static str> {
    (m & (MOD_CTRL | MOD_ALT | MOD_META) == 0).then_some(t)
}

const LETTER_CODES: [&str; 26] = [
    "KeyA", "KeyB", "KeyC", "KeyD", "KeyE", "KeyF", "KeyG", "KeyH", "KeyI", "KeyJ", "KeyK", "KeyL",
    "KeyM", "KeyN", "KeyO", "KeyP", "KeyQ", "KeyR", "KeyS", "KeyT", "KeyU", "KeyV", "KeyW", "KeyX",
    "KeyY", "KeyZ",
];

const DIGIT_CODES: [&str; 10] = [
    "Digit0", "Digit1", "Digit2", "Digit3", "Digit4", "Digit5", "Digit6", "Digit7", "Digit8",
    "Digit9",
];

/// `(code, vk)` for a printable ASCII character on a US layout.
fn ascii_position(c: char) -> Option<(&'static str, u32)> {
    const SHIFTED_DIGITS: &str = ")!@#$%^&*(";
    const PUNCT: &[(char, char, &str, u32)] = &[
        ('`', '~', "Backquote", 192),
        ('-', '_', "Minus", 189),
        ('=', '+', "Equal", 187),
        ('[', '{', "BracketLeft", 219),
        (']', '}', "BracketRight", 221),
        ('\\', '|', "Backslash", 220),
        (';', ':', "Semicolon", 186),
        ('\'', '"', "Quote", 222),
        (',', '<', "Comma", 188),
        ('.', '>', "Period", 190),
        ('/', '?', "Slash", 191),
    ];
    match c {
        'a'..='z' | 'A'..='Z' => {
            let up = c.to_ascii_uppercase();
            Some((LETTER_CODES[(up as u8 - b'A') as usize], up as u32))
        }
        '0'..='9' => Some((DIGIT_CODES[(c as u8 - b'0') as usize], c as u32)),
        ' ' => Some(("Space", 32)),
        _ => {
            if let Some(i) = SHIFTED_DIGITS.find(c) {
                return Some((DIGIT_CODES[i], b'0' as u32 + i as u32));
            }
            PUNCT
                .iter()
                .find(|(a, b, _, _)| *a == c || *b == c)
                .map(|(_, _, code, vk)| (*code, *vk))
        }
    }
}

fn translate_char(c: char, m: u8) -> Option<CdpKey> {
    let ctrl = m & MOD_CTRL != 0;
    let shift = m & MOD_SHIFT != 0;
    let alt = m & MOD_ALT != 0;
    let meta = m & MOD_META != 0;

    // Control characters: WezTerm sends Enter/Tab/Backspace/Escape/Delete as `Char`s when they
    // do not arrive as the named keys, and Ctrl+letter may arrive already mapped to C0.
    match c {
        '\r' | '\n' => return translate(&KeyCode::Enter, mod_bits(m)),
        '\t' => return translate(&KeyCode::Tab, mod_bits(m)),
        '\u{8}' => return translate(&KeyCode::Backspace, mod_bits(m)),
        '\u{1b}' => return translate(&KeyCode::Escape, mod_bits(m)),
        '\u{7f}' => return translate(&KeyCode::Delete, mod_bits(m)),
        '\u{1}'..='\u{1a}' if ctrl => {
            return translate_char(char::from(b'a' + (c as u8 - 1)), m);
        }
        c if c.is_control() => return None,
        _ => {}
    }

    let chord = ctrl || alt || meta;
    let (code, vk) = ascii_position(c).unwrap_or(("", 0));
    let commands: &'static [&'static str] = if ctrl && !alt && !meta {
        match c.to_ascii_lowercase() {
            'a' => &["selectAll"],
            'c' => &["copy"],
            'x' => &["cut"],
            'v' => &["paste"],
            'z' if shift => &["redo"],
            'z' => &["undo"],
            'y' => &["redo"],
            _ => &[],
        }
    } else {
        &[]
    };
    Some(CdpKey {
        key: c.to_string(),
        code,
        vk,
        text: (!chord).then(|| c.to_string()),
        location: 0,
        modifiers: m,
        commands,
    })
}

/// Back from CDP bits to termwiz modifiers, for re-entering `translate` with a named key.
fn mod_bits(m: u8) -> Modifiers {
    let mut out = Modifiers::NONE;
    if m & MOD_ALT != 0 {
        out |= Modifiers::ALT;
    }
    if m & MOD_CTRL != 0 {
        out |= Modifiers::CTRL;
    }
    if m & MOD_META != 0 {
        out |= Modifiers::SUPER;
    }
    if m & MOD_SHIFT != 0 {
        out |= Modifiers::SHIFT;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(key: KeyCode, mods: Modifiers) -> CdpKey {
        translate(&key, mods).unwrap_or_else(|| panic!("{key:?} untranslated"))
    }

    #[test]
    fn printable_characters() {
        let a = t(KeyCode::Char('a'), Modifiers::NONE);
        assert_eq!((a.key.as_str(), a.code, a.vk), ("a", "KeyA", 65));
        assert_eq!(a.text.as_deref(), Some("a"));
        assert_eq!(a.params(true)["type"], "keyDown");
        assert_eq!(a.params(true)["text"], "a");

        let shifted = t(KeyCode::Char('A'), Modifiers::SHIFT);
        assert_eq!((shifted.key.as_str(), shifted.code, shifted.vk), ("A", "KeyA", 65));
        assert_eq!(shifted.text.as_deref(), Some("A"));
        assert_eq!(shifted.modifiers, MOD_SHIFT);

        let d = t(KeyCode::Char('7'), Modifiers::NONE);
        assert_eq!((d.code, d.vk), ("Digit7", 55));
        let bang = t(KeyCode::Char('!'), Modifiers::SHIFT);
        assert_eq!((bang.code, bang.vk, bang.text.as_deref()), ("Digit1", 49, Some("!")));
        let semi = t(KeyCode::Char(';'), Modifiers::NONE);
        assert_eq!((semi.code, semi.vk), ("Semicolon", 186));
        let colon = t(KeyCode::Char(':'), Modifiers::SHIFT);
        assert_eq!((colon.code, colon.vk), ("Semicolon", 186));
        let space = t(KeyCode::Char(' '), Modifiers::NONE);
        assert_eq!((space.code, space.vk, space.text.as_deref()), ("Space", 32, Some(" ")));
        let e = t(KeyCode::Char('é'), Modifiers::NONE);
        assert_eq!((e.key.as_str(), e.code, e.vk, e.text.as_deref()), ("é", "", 0, Some("é")));
        assert!(e.params(true).get("code").is_none());
    }

    #[test]
    fn every_punctuation_key_has_a_position() {
        for c in "`-=[]\\;',./~_+{}|:\"<>?".chars() {
            let k = t(KeyCode::Char(c), Modifiers::NONE);
            assert!(!k.code.is_empty() && k.vk != 0, "{c}");
        }
        for c in ")!@#$%^&*(".chars() {
            assert!(t(KeyCode::Char(c), Modifiers::SHIFT).code.starts_with("Digit"), "{c}");
        }
    }

    #[test]
    fn named_keys() {
        let cases = [
            (KeyCode::Enter, "Enter", "Enter", 13),
            (KeyCode::Tab, "Tab", "Tab", 9),
            (KeyCode::Backspace, "Backspace", "Backspace", 8),
            (KeyCode::Escape, "Escape", "Escape", 27),
            (KeyCode::Delete, "Delete", "Delete", 46),
            (KeyCode::Insert, "Insert", "Insert", 45),
            (KeyCode::Home, "Home", "Home", 36),
            (KeyCode::End, "End", "End", 35),
            (KeyCode::PageUp, "PageUp", "PageUp", 33),
            (KeyCode::PageDown, "PageDown", "PageDown", 34),
            (KeyCode::LeftArrow, "ArrowLeft", "ArrowLeft", 37),
            (KeyCode::UpArrow, "ArrowUp", "ArrowUp", 38),
            (KeyCode::RightArrow, "ArrowRight", "ArrowRight", 39),
            (KeyCode::DownArrow, "ArrowDown", "ArrowDown", 40),
        ];
        for (kc, key, code, vk) in cases {
            let k = t(kc, Modifiers::NONE);
            assert_eq!((k.key.as_str(), k.code, k.vk), (key, code, vk), "{kc:?}");
        }
        assert_eq!(t(KeyCode::Enter, Modifiers::NONE).text.as_deref(), Some("\r"));
        assert_eq!(t(KeyCode::Enter, Modifiers::NONE).params(true)["type"], "keyDown");
        assert_eq!(t(KeyCode::Tab, Modifiers::NONE).params(true)["type"], "rawKeyDown");
        assert_eq!(t(KeyCode::LeftArrow, Modifiers::NONE).params(true)["type"], "rawKeyDown");
    }

    #[test]
    fn function_keys() {
        for n in 1u8..=24 {
            let k = t(KeyCode::Function(n), Modifiers::NONE);
            assert_eq!(k.key, format!("F{n}"));
            assert_eq!(k.code, format!("F{n}"));
            assert_eq!(k.vk, 111 + n as u32);
        }
        assert_eq!(t(KeyCode::Function(1), Modifiers::NONE).vk, 112);
        assert_eq!(t(KeyCode::Function(12), Modifiers::NONE).vk, 123);
        assert!(translate(&KeyCode::Function(0), Modifiers::NONE).is_none());
        assert!(translate(&KeyCode::Function(25), Modifiers::NONE).is_none());
    }

    #[test]
    fn modifiers_map_to_cdp_bits() {
        assert_eq!(cdp_modifiers(Modifiers::NONE), 0);
        assert_eq!(cdp_modifiers(Modifiers::ALT), MOD_ALT);
        assert_eq!(cdp_modifiers(Modifiers::LEFT_ALT), MOD_ALT);
        assert_eq!(cdp_modifiers(Modifiers::RIGHT_ALT), MOD_ALT);
        assert_eq!(cdp_modifiers(Modifiers::CTRL), MOD_CTRL);
        assert_eq!(cdp_modifiers(Modifiers::RIGHT_CTRL), MOD_CTRL);
        assert_eq!(cdp_modifiers(Modifiers::SUPER), MOD_META);
        assert_eq!(cdp_modifiers(Modifiers::SHIFT), MOD_SHIFT);
        assert_eq!(cdp_modifiers(Modifiers::LEFT_SHIFT), MOD_SHIFT);
        assert_eq!(
            cdp_modifiers(Modifiers::ALT | Modifiers::CTRL | Modifiers::SUPER | Modifiers::SHIFT),
            15
        );
        // WezTerm-internal bits are not the page's business.
        assert_eq!(cdp_modifiers(Modifiers::LEADER | Modifiers::ENHANCED_KEY), 0);
        let k = t(KeyCode::UpArrow, Modifiers::CTRL | Modifiers::SHIFT);
        assert_eq!(k.params(true)["modifiers"], 10);
    }

    #[test]
    fn chords_do_not_type_and_carry_editing_commands() {
        let a = t(KeyCode::Char('a'), Modifiers::CTRL);
        assert_eq!(a.text, None);
        assert_eq!(a.commands, ["selectAll"]);
        let p = a.params(true);
        assert_eq!(p["type"], "rawKeyDown");
        assert_eq!(p["commands"][0], "selectAll");
        assert_eq!(p["modifiers"], 2);
        assert!(p.get("text").is_none());

        assert_eq!(t(KeyCode::Char('c'), Modifiers::CTRL).commands, ["copy"]);
        assert_eq!(t(KeyCode::Char('x'), Modifiers::CTRL).commands, ["cut"]);
        assert_eq!(t(KeyCode::Char('v'), Modifiers::CTRL).commands, ["paste"]);
        assert_eq!(t(KeyCode::Char('z'), Modifiers::CTRL).commands, ["undo"]);
        assert_eq!(t(KeyCode::Char('z'), Modifiers::CTRL | Modifiers::SHIFT).commands, ["redo"]);
        assert_eq!(t(KeyCode::Char('y'), Modifiers::CTRL).commands, ["redo"]);
        assert_eq!(t(KeyCode::Insert, Modifiers::CTRL).commands, ["copy"]);
        assert_eq!(t(KeyCode::Insert, Modifiers::SHIFT).commands, ["paste"]);
        assert_eq!(t(KeyCode::Delete, Modifiers::SHIFT).commands, ["cut"]);
        assert!(t(KeyCode::Char('a'), Modifiers::NONE).commands.is_empty());
        assert!(t(KeyCode::Char('q'), Modifiers::CTRL).commands.is_empty());
        // Alt chords type nothing either.
        assert_eq!(t(KeyCode::Char('x'), Modifiers::ALT).text, None);
        assert!(t(KeyCode::Char('x'), Modifiers::ALT).commands.is_empty());
        // Release carries neither text nor commands.
        let up = a.params(false);
        assert_eq!(up["type"], "keyUp");
        assert!(up.get("commands").is_none());
    }

    #[test]
    fn control_characters_map_back_to_keys() {
        assert_eq!(t(KeyCode::Char('\r'), Modifiers::NONE).key, "Enter");
        assert_eq!(t(KeyCode::Char('\t'), Modifiers::NONE).key, "Tab");
        assert_eq!(t(KeyCode::Char('\u{8}'), Modifiers::NONE).key, "Backspace");
        assert_eq!(t(KeyCode::Char('\u{1b}'), Modifiers::NONE).key, "Escape");
        assert_eq!(t(KeyCode::Char('\u{7f}'), Modifiers::NONE).key, "Delete");
        let c0 = t(KeyCode::Char('\u{1}'), Modifiers::CTRL);
        assert_eq!((c0.key.as_str(), c0.commands), ("a", &["selectAll"][..]));
        assert!(translate(&KeyCode::Char('\u{1}'), Modifiers::NONE).is_none());
    }

    #[test]
    fn numpad_keys() {
        let k = t(KeyCode::Numpad5, Modifiers::NONE);
        assert_eq!((k.key.as_str(), k.code, k.vk, k.location), ("5", "Numpad5", 101, 3));
        assert_eq!(k.text.as_deref(), Some("5"));
        assert_eq!(k.params(true)["isKeypad"], true);
        let add = t(KeyCode::Add, Modifiers::NONE);
        assert_eq!((add.code, add.vk), ("NumpadAdd", 107));
        assert_eq!(t(KeyCode::Divide, Modifiers::CTRL).text, None);
    }

    #[test]
    fn modifier_only_and_media_keys_are_not_forwarded() {
        for k in [
            KeyCode::Shift,
            KeyCode::LeftControl,
            KeyCode::Alt,
            KeyCode::Super,
            KeyCode::VolumeUp,
            KeyCode::MediaPlayPause,
            KeyCode::BrowserBack,
        ] {
            assert!(translate(&k, Modifiers::NONE).is_none(), "{k:?}");
        }
    }
}
