//! Keys, sans IO: the app's [`Key`] (the Mac virtual key code and NSEvent's characters) as the key events CEF's
//! off-screen browser wants, filled as cefclient's macOS OSR view fills them (browser_window_osr_mac.mm): the Windows
//! key code from the Mac code (Chromium's `KeyboardCodeFromKeyCode` table, ui/events/keycodes/keyboard_code_conversion_mac),
//! the Mac code as `native_key_code`, the first UTF-16 unit of `characters` / `charactersIgnoringModifiers` as
//! `character` / `unmodified_character`; `Down` is RAWKEYDOWN, `Up` KEYUP, and `Char` one CHAR per UTF-16 unit typed.
use harness_proto::browser::{Key, KeyKind};

/// CEF's key event types this host sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    RawKeyDown,
    KeyUp,
    Char,
}

/// One CEF key event's fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fields {
    pub kind: Kind,
    pub modifiers: u32,
    pub windows_key_code: i32,
    pub native_key_code: i32,
    pub character: u16,
    pub unmodified_character: u16,
}

/// Windows virtual-key codes (ui/events/keycodes/keyboard_codes_posix.h).
mod vk {
    pub const BACK: i32 = 0x08;
    pub const TAB: i32 = 0x09;
    pub const CLEAR: i32 = 0x0C;
    pub const RETURN: i32 = 0x0D;
    pub const SHIFT: i32 = 0x10;
    pub const CONTROL: i32 = 0x11;
    pub const MENU: i32 = 0x12;
    pub const CAPITAL: i32 = 0x14;
    pub const KANA: i32 = 0x15;
    pub const NONCONVERT: i32 = 0x1D;
    pub const ESCAPE: i32 = 0x1B;
    pub const SPACE: i32 = 0x20;
    pub const PRIOR: i32 = 0x21;
    pub const NEXT: i32 = 0x22;
    pub const END: i32 = 0x23;
    pub const HOME: i32 = 0x24;
    pub const LEFT: i32 = 0x25;
    pub const UP: i32 = 0x26;
    pub const RIGHT: i32 = 0x27;
    pub const DOWN: i32 = 0x28;
    pub const INSERT: i32 = 0x2D;
    pub const DELETE: i32 = 0x2E;
    pub const LWIN: i32 = 0x5B;
    pub const RWIN: i32 = 0x5C;
    pub const APPS: i32 = 0x5D;
    pub const NUMPAD0: i32 = 0x60;
    pub const MULTIPLY: i32 = 0x6A;
    pub const ADD: i32 = 0x6B;
    pub const SUBTRACT: i32 = 0x6D;
    pub const DECIMAL: i32 = 0x6E;
    pub const DIVIDE: i32 = 0x6F;
    pub const F1: i32 = 0x70;
    pub const VOLUME_MUTE: i32 = 0xAD;
    pub const VOLUME_DOWN: i32 = 0xAE;
    pub const VOLUME_UP: i32 = 0xAF;
    pub const OEM_1: i32 = 0xBA;
    pub const OEM_PLUS: i32 = 0xBB;
    pub const OEM_COMMA: i32 = 0xBC;
    pub const OEM_MINUS: i32 = 0xBD;
    pub const OEM_PERIOD: i32 = 0xBE;
    pub const OEM_2: i32 = 0xBF;
    pub const OEM_3: i32 = 0xC0;
    pub const OEM_4: i32 = 0xDB;
    pub const OEM_5: i32 = 0xDC;
    pub const OEM_6: i32 = 0xDD;
    pub const OEM_7: i32 = 0xDE;
    pub const OEM_102: i32 = 0xE2;
}

const fn f(n: i32) -> i32 {
    vk::F1 + n - 1
}

const fn ch(c: u8) -> i32 {
    c as i32
}

/// The Windows key code for Mac virtual key code `code` (Carbon's `kVK_*`); None for a code no key has.
#[must_use]
pub fn windows_code(code: u16) -> Option<i32> {
    use vk::*;
    Some(match code {
        0x00 => ch(b'A'),
        0x01 => ch(b'S'),
        0x02 => ch(b'D'),
        0x03 => ch(b'F'),
        0x04 => ch(b'H'),
        0x05 => ch(b'G'),
        0x06 => ch(b'Z'),
        0x07 => ch(b'X'),
        0x08 => ch(b'C'),
        0x09 => ch(b'V'),
        0x0A => OEM_3, // kVK_ISO_Section
        0x0B => ch(b'B'),
        0x0C => ch(b'Q'),
        0x0D => ch(b'W'),
        0x0E => ch(b'E'),
        0x0F => ch(b'R'),
        0x10 => ch(b'Y'),
        0x11 => ch(b'T'),
        0x12 => ch(b'1'),
        0x13 => ch(b'2'),
        0x14 => ch(b'3'),
        0x15 => ch(b'4'),
        0x16 => ch(b'6'),
        0x17 => ch(b'5'),
        0x18 => OEM_PLUS,
        0x19 => ch(b'9'),
        0x1A => ch(b'7'),
        0x1B => OEM_MINUS,
        0x1C => ch(b'8'),
        0x1D => ch(b'0'),
        0x1E => OEM_6,
        0x1F => ch(b'O'),
        0x20 => ch(b'U'),
        0x21 => OEM_4,
        0x22 => ch(b'I'),
        0x23 => ch(b'P'),
        0x24 => RETURN,
        0x25 => ch(b'L'),
        0x26 => ch(b'J'),
        0x27 => OEM_7,
        0x28 => ch(b'K'),
        0x29 => OEM_1,
        0x2A => OEM_5,
        0x2B => OEM_COMMA,
        0x2C => OEM_2,
        0x2D => ch(b'N'),
        0x2E => ch(b'M'),
        0x2F => OEM_PERIOD,
        0x30 => TAB,
        0x31 => SPACE,
        0x32 => OEM_3,
        0x33 => BACK,
        0x35 => ESCAPE,
        0x36 => RWIN,
        0x37 => LWIN,
        0x38 | 0x3C => SHIFT,
        0x39 => CAPITAL,
        0x3A | 0x3D => MENU,
        0x3B | 0x3E => CONTROL,
        0x40 => f(17),
        0x41 => DECIMAL,
        0x43 => MULTIPLY,
        0x45 => ADD,
        0x47 => CLEAR,
        0x48 => VOLUME_UP,
        0x49 => VOLUME_DOWN,
        0x4A => VOLUME_MUTE,
        0x4B => DIVIDE,
        0x4C => RETURN, // keypad enter
        0x4E => SUBTRACT,
        0x4F => f(18),
        0x50 => f(19),
        0x51 => OEM_PLUS, // keypad equals
        0x52..=0x59 => NUMPAD0 + i32::from(code - 0x52),
        0x5A => f(20),
        0x5B => NUMPAD0 + 8,
        0x5C => NUMPAD0 + 9,
        0x5D => OEM_5,     // kVK_JIS_Yen
        0x5E => OEM_102,   // kVK_JIS_Underscore
        0x5F => OEM_COMMA, // kVK_JIS_KeypadComma
        0x60 => f(5),
        0x61 => f(6),
        0x62 => f(7),
        0x63 => f(3),
        0x64 => f(8),
        0x65 => f(9),
        0x66 => NONCONVERT, // kVK_JIS_Eisu
        0x67 => f(11),
        0x68 => KANA, // kVK_JIS_Kana
        0x69 => f(13),
        0x6A => f(16),
        0x6B => f(14),
        0x6D => f(10),
        0x6E => APPS,
        0x6F => f(12),
        0x71 => f(15),
        0x72 => INSERT, // kVK_Help
        0x73 => HOME,
        0x74 => PRIOR,
        0x75 => DELETE, // forward delete
        0x76 => f(4),
        0x77 => END,
        0x78 => f(2),
        0x79 => NEXT,
        0x7A => f(1),
        0x7B => LEFT,
        0x7C => RIGHT,
        0x7D => DOWN,
        0x7E => UP,
        _ => return None,
    })
}

/// The CEF key events for one app key event.
///
/// # Errors
/// A Mac key code no key has, naming it.
pub fn events(k: &Key) -> Result<Vec<Fields>, String> {
    let windows_key_code = windows_code(k.code).ok_or_else(|| format!("Mac key code 0x{:02x} is no key harness-browser knows (keys.rs)", k.code))?;
    let base = Fields {
        kind: Kind::RawKeyDown,
        modifiers: k.mods,
        windows_key_code,
        native_key_code: i32::from(k.code),
        character: k.chars.encode_utf16().next().unwrap_or(0),
        unmodified_character: k.unmod.encode_utf16().next().unwrap_or(0),
    };
    Ok(match k.kind {
        KeyKind::Down => vec![base],                                        // alloc: one event
        KeyKind::Up => vec![Fields { kind: Kind::KeyUp, ..base }], // alloc: one event
        KeyKind::Char => {
            let unmod: Vec<u16> = k.unmod.encode_utf16().collect(); // alloc: one key's characters
            k.chars
                .encode_utf16()
                .enumerate()
                .map(|(i, u)| Fields { kind: Kind::Char, character: u, unmodified_character: unmod.get(i).copied().unwrap_or(u), ..base })
                .collect()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(kind: KeyKind, code: u16, chars: &str, unmod: &str) -> Key {
        Key { kind, code, chars: chars.into(), unmod: unmod.into(), mods: 0 }
    }

    #[test]
    fn the_table() {
        for (mac, win) in [
            (0x00, 0x41), // A
            (0x06, 0x5A), // Z
            (0x1D, 0x30), // 0
            (0x12, 0x31), // 1
            (0x24, 0x0D), // Return
            (0x30, 0x09), // Tab
            (0x31, 0x20), // Space
            (0x33, 0x08), // Delete (backspace)
            (0x75, 0x2E), // Forward delete
            (0x35, 0x1B), // Escape
            (0x7B, 0x25), // Left
            (0x7E, 0x26), // Up
            (0x7A, 0x70), // F1
            (0x6F, 0x7B), // F12
            (0x52, 0x60), // Keypad 0
            (0x5C, 0x69), // Keypad 9
            (0x37, 0x5B), // Command
            (0x2F, 0xBE), // .
            (0x32, 0xC0), // `
        ] {
            assert_eq!(windows_code(mac), Some(win), "Mac key code 0x{mac:02x}");
        }
        assert_eq!(windows_code(0x34), None);
        assert_eq!(windows_code(0x80), None);
    }

    #[test]
    fn down_up_and_chars() {
        let d = events(&key(KeyKind::Down, 0x00, "a", "a")).unwrap();
        assert_eq!(d, vec![Fields { kind: Kind::RawKeyDown, modifiers: 0, windows_key_code: 0x41, native_key_code: 0, character: 'a' as u16, unmodified_character: 'a' as u16 }]);
        assert_eq!(events(&key(KeyKind::Up, 0x00, "a", "a")).unwrap()[0].kind, Kind::KeyUp);
        // An emoji is two UTF-16 units: two CHARs.
        let c = events(&key(KeyKind::Char, 0x00, "😀", "a")).unwrap();
        assert_eq!(c.len(), 2);
        assert!(c.iter().all(|e| e.kind == Kind::Char));
        assert_eq!(c[0].unmodified_character, 'a' as u16);
        assert!(events(&key(KeyKind::Down, 0x34, "", "")).unwrap_err().contains("0x34"));
    }
}
