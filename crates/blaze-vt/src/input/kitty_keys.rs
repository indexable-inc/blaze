//! The kitty keyboard protocol's functional keys, ghostty's `src/input/kitty.zig` (ported from
//! foot's `kitty-keymap.h`; <https://sw.kovidgoyal.net/kitty/keyboard-protocol/#functional-key-definitions>),
//! in the order of that page.

use super::key::Key;

/// A key's kitty code, the final byte its sequence ends with, and whether it is a modifier key
/// (sent only under "report all").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Entry {
    pub(crate) code: u32,
    pub(crate) final_byte: u8,
    pub(crate) modifier: bool,
}

const fn e(code: u32, final_byte: u8) -> Option<Entry> {
    Some(Entry { code, final_byte, modifier: false })
}

const fn modifier(code: u32) -> Option<Entry> {
    Some(Entry { code, final_byte: b'u', modifier: true })
}

/// The table entry for `key`, if it has one (ghostty `kitty.zig:39`).
pub(crate) const fn entry(key: Key) -> Option<Entry> {
    use Key::*;
    match key {
        Escape => e(27, b'u'),
        Enter => e(13, b'u'),
        Tab => e(9, b'u'),
        Backspace => e(127, b'u'),
        Insert => e(2, b'~'),
        Delete => e(3, b'~'),
        ArrowLeft => e(1, b'D'),
        ArrowRight => e(1, b'C'),
        ArrowUp => e(1, b'A'),
        ArrowDown => e(1, b'B'),
        PageUp => e(5, b'~'),
        PageDown => e(6, b'~'),
        Home => e(1, b'H'),
        End => e(1, b'F'),
        CapsLock => modifier(57358),
        ScrollLock => e(57359, b'u'),
        NumLock => modifier(57360),
        PrintScreen => e(57361, b'u'),
        Pause => e(57362, b'u'),

        F1 => e(1, b'P'),
        F2 => e(1, b'Q'),
        F3 => e(13, b'~'),
        F4 => e(1, b'S'),
        F5 => e(15, b'~'),
        F6 => e(17, b'~'),
        F7 => e(18, b'~'),
        F8 => e(19, b'~'),
        F9 => e(20, b'~'),
        F10 => e(21, b'~'),
        F11 => e(23, b'~'),
        F12 => e(24, b'~'),
        F13 => e(57376, b'u'),
        F14 => e(57377, b'u'),
        F15 => e(57378, b'u'),
        F16 => e(57379, b'u'),
        F17 => e(57380, b'u'),
        F18 => e(57381, b'u'),
        F19 => e(57382, b'u'),
        F20 => e(57383, b'u'),
        F21 => e(57384, b'u'),
        F22 => e(57385, b'u'),
        F23 => e(57386, b'u'),
        F24 => e(57387, b'u'),
        F25 => e(57388, b'u'),

        Numpad0 => e(57399, b'u'),
        Numpad1 => e(57400, b'u'),
        Numpad2 => e(57401, b'u'),
        Numpad3 => e(57402, b'u'),
        Numpad4 => e(57403, b'u'),
        Numpad5 => e(57404, b'u'),
        Numpad6 => e(57405, b'u'),
        Numpad7 => e(57406, b'u'),
        Numpad8 => e(57407, b'u'),
        Numpad9 => e(57408, b'u'),
        NumpadDecimal => e(57409, b'u'),
        NumpadDivide => e(57410, b'u'),
        NumpadMultiply => e(57411, b'u'),
        NumpadSubtract => e(57412, b'u'),
        NumpadAdd => e(57413, b'u'),
        NumpadEnter => e(57414, b'u'),
        NumpadEqual => e(57415, b'u'),
        NumpadSeparator => e(57416, b'u'),
        NumpadLeft => e(57417, b'u'),
        NumpadRight => e(57418, b'u'),
        NumpadUp => e(57419, b'u'),
        NumpadDown => e(57420, b'u'),
        NumpadPageUp => e(57421, b'u'),
        NumpadPageDown => e(57422, b'u'),
        NumpadHome => e(57423, b'u'),
        NumpadEnd => e(57424, b'u'),
        NumpadInsert => e(57425, b'u'),
        NumpadDelete => e(57426, b'u'),
        NumpadBegin => e(57427, b'u'),

        ShiftLeft => modifier(57441),
        ShiftRight => modifier(57447),
        ControlLeft => modifier(57442),
        ControlRight => modifier(57448),
        MetaLeft => modifier(57444),
        MetaRight => modifier(57450),
        AltLeft => modifier(57443),
        AltRight => modifier(57449),

        _ => None,
    }
}
