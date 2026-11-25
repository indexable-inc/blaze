//! xterm's "PC-style function keys" for the legacy protocols, ghostty's
//! `src/input/function_keys.zig` (itself mostly foot's `keymap.h`). These win over everything
//! else in legacy encoding: even modern protocols keep them.
//!
//! ghostty expands each `pcStyle(fmt)` into fifteen entries, one per non-empty modifier set,
//! each with its parameter (2–16) in the `{}` hole. Here that run is one entry,
//! [`ModsMatch::EachModifier`], which matches exactly when one of the fifteen would and fills
//! the hole the same way; every run sits where ghostty's does, so the first match is the same.

use super::key::{Key, Mods};
use super::{BufferFull, Out};

/// What DECCKM must be for an entry to apply.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CursorMode {
    Any,
    Normal,
    Application,
}

/// What the keypad mode (66, less 1035) must be. (ghostty's enum has a `normal` too, which no
/// entry uses.)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum KeypadMode {
    Any,
    Application,
}

/// The modifyOtherKeys state an entry needs: `Set` is "not mode 2", `SetOther` is mode 2.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ModifyKeys {
    Any,
    Set,
    SetOther,
}

/// Which binding modifiers an entry takes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ModsMatch {
    /// Any modifiers (ghostty: empty mods, `mods_empty_is_any`).
    Any,
    /// No modifiers (empty mods, `mods_empty_is_any = false`).
    None,
    /// Exactly these.
    Exact(Mods),
    /// Any non-empty set; its legacy parameter fills the sequence's `{}`.
    EachModifier,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Entry {
    pub(crate) mods: ModsMatch,
    pub(crate) cursor: CursorMode,
    pub(crate) keypad: KeypadMode,
    pub(crate) modify_other_keys: ModifyKeys,
    /// The bytes, with a `{}` hole for [`ModsMatch::EachModifier`].
    pub(crate) sequence: &'static [u8],
    /// The bytes when DECBKM is set.
    pub(crate) decbkm: Option<&'static [u8]>,
}

/// A matched entry's bytes and, for a modifier run, the parameter for its hole.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Sequence {
    pub(crate) bytes: &'static [u8],
    pub(crate) param: Option<u8>,
}

impl Sequence {
    pub(crate) fn write(self, w: &mut Out<'_>) -> Result<(), BufferFull> {
        let Some(param) = self.param else {
            return w.bytes(self.bytes);
        };
        let hole = self.bytes.windows(2).position(|p| p == b"{}").unwrap_or(self.bytes.len());
        w.bytes(&self.bytes[..hole])?;
        w.dec(u32::from(param))?;
        w.bytes(self.bytes.get(hole + 2..).unwrap_or(&[]))
    }
}

const BASE: Entry = Entry {
    mods: ModsMatch::Any,
    cursor: CursorMode::Any,
    keypad: KeypadMode::Any,
    modify_other_keys: ModifyKeys::Any,
    sequence: b"",
    decbkm: None,
};

const S: Mods = Mods::SHIFT;
const A: Mods = Mods::ALT;
const C: Mods = Mods::CTRL;
const U: Mods = Mods::SUPER;

/// A modifier set from its letters: `m!(S A)` is shift+alt.
macro_rules! m {
    ($($x:ident)+) => { Mods::NONE $(.union($x))+ };
}

/// Any modifiers.
const fn any(sequence: &'static [u8]) -> Entry {
    Entry { sequence, ..BASE }
}

/// Exactly `mods`.
const fn exact(mods: Mods, sequence: &'static [u8]) -> Entry {
    Entry { mods: ModsMatch::Exact(mods), sequence, ..BASE }
}

/// Exactly `mods`, when modifyOtherKeys is not 2.
const fn set(mods: Mods, sequence: &'static [u8]) -> Entry {
    Entry { modify_other_keys: ModifyKeys::Set, ..exact(mods, sequence) }
}

/// Exactly `mods`, under modifyOtherKeys 2.
const fn set_other(mods: Mods, sequence: &'static [u8]) -> Entry {
    Entry { modify_other_keys: ModifyKeys::SetOther, ..exact(mods, sequence) }
}

/// ghostty `pcStyle(fmt)`: the fifteen modifier entries.
const fn pc(fmt: &'static [u8]) -> Entry {
    Entry { mods: ModsMatch::EachModifier, sequence: fmt, ..BASE }
}

/// ghostty `cursorKey(normal, application)`.
const fn cursor(normal: &'static [u8], application: &'static [u8]) -> [Entry; 2] {
    [
        Entry { cursor: CursorMode::Normal, sequence: normal, ..BASE },
        Entry { cursor: CursorMode::Application, sequence: application, ..BASE },
    ]
}

/// ghostty `pcStyle(fmt) ++ cursorKey(normal, application)`.
const fn cursor_key(
    fmt: &'static [u8],
    normal: &'static [u8],
    application: &'static [u8],
) -> [Entry; 3] {
    let [n, a] = cursor(normal, application);
    [pc(fmt), n, a]
}

/// ghostty `kpKeys(suffix)`: in application keypad mode, `SS3 x` unmodified (`kpDefault`),
/// then `SS3 m x` with modifiers.
const fn kp(plain: &'static [u8], modified: &'static [u8]) -> [Entry; 2] {
    [
        Entry { mods: ModsMatch::None, keypad: KeypadMode::Application, sequence: plain, ..BASE },
        Entry { keypad: KeypadMode::Application, ..pc(modified) },
    ]
}

const ARROW_UP: [Entry; 3] = cursor_key(b"\x1b[1;{}A", b"\x1b[A", b"\x1bOA");
const ARROW_DOWN: [Entry; 3] = cursor_key(b"\x1b[1;{}B", b"\x1b[B", b"\x1bOB");
const ARROW_RIGHT: [Entry; 3] = cursor_key(b"\x1b[1;{}C", b"\x1b[C", b"\x1bOC");
const ARROW_LEFT: [Entry; 3] = cursor_key(b"\x1b[1;{}D", b"\x1b[D", b"\x1bOD");
const BEGIN: [Entry; 3] = cursor_key(b"\x1b[1;{}E", b"\x1b[E", b"\x1bOE");
const HOME: [Entry; 3] = cursor_key(b"\x1b[1;{}H", b"\x1b[H", b"\x1bOH");
const END: [Entry; 3] = cursor_key(b"\x1b[1;{}F", b"\x1b[F", b"\x1bOF");
const INSERT: [Entry; 2] = [pc(b"\x1b[2;{}~"), any(b"\x1B[2~")];
const DELETE: [Entry; 2] = [pc(b"\x1b[3;{}~"), any(b"\x1B[3~")];
const PAGE_UP: [Entry; 2] = [pc(b"\x1b[5;{}~"), any(b"\x1B[5~")];
const PAGE_DOWN: [Entry; 2] = [pc(b"\x1b[6;{}~"), any(b"\x1B[6~")];

// Function keys. F13 and above have no legacy encoding (ghostty's todo).
const F1: [Entry; 2] = [pc(b"\x1b[1;{}P"), any(b"\x1BOP")];
const F2: [Entry; 2] = [pc(b"\x1b[1;{}Q"), any(b"\x1BOQ")];
const F3: [Entry; 2] = [pc(b"\x1b[13;{}~"), any(b"\x1BOR")];
const F4: [Entry; 2] = [pc(b"\x1b[1;{}S"), any(b"\x1BOS")];
const F5: [Entry; 2] = [pc(b"\x1b[15;{}~"), any(b"\x1B[15~")];
const F6: [Entry; 2] = [pc(b"\x1b[17;{}~"), any(b"\x1B[17~")];
const F7: [Entry; 2] = [pc(b"\x1b[18;{}~"), any(b"\x1B[18~")];
const F8: [Entry; 2] = [pc(b"\x1b[19;{}~"), any(b"\x1B[19~")];
const F9: [Entry; 2] = [pc(b"\x1b[20;{}~"), any(b"\x1B[20~")];
const F10: [Entry; 2] = [pc(b"\x1b[21;{}~"), any(b"\x1B[21~")];
const F11: [Entry; 2] = [pc(b"\x1b[23;{}~"), any(b"\x1B[23~")];
const F12: [Entry; 2] = [pc(b"\x1b[24;{}~"), any(b"\x1B[24~")];

// Keypad keys.
const KP_0: [Entry; 2] = kp(b"\x1bOp", b"\x1bO{}p");
const KP_1: [Entry; 2] = kp(b"\x1bOq", b"\x1bO{}q");
const KP_2: [Entry; 2] = kp(b"\x1bOr", b"\x1bO{}r");
const KP_3: [Entry; 2] = kp(b"\x1bOs", b"\x1bO{}s");
const KP_4: [Entry; 2] = kp(b"\x1bOt", b"\x1bO{}t");
const KP_5: [Entry; 2] = kp(b"\x1bOu", b"\x1bO{}u");
const KP_6: [Entry; 2] = kp(b"\x1bOv", b"\x1bO{}v");
const KP_7: [Entry; 2] = kp(b"\x1bOw", b"\x1bO{}w");
const KP_8: [Entry; 2] = kp(b"\x1bOx", b"\x1bO{}x");
const KP_9: [Entry; 2] = kp(b"\x1bOy", b"\x1bO{}y");
const KP_DECIMAL: [Entry; 2] = kp(b"\x1bOn", b"\x1bO{}n");
const KP_DIVIDE: [Entry; 2] = kp(b"\x1bOo", b"\x1bO{}o");
const KP_MULTIPLY: [Entry; 2] = kp(b"\x1bOj", b"\x1bO{}j");
const KP_SUBTRACT: [Entry; 2] = kp(b"\x1bOm", b"\x1bO{}m");
const KP_ADD: [Entry; 2] = kp(b"\x1bOk", b"\x1bO{}k");
const KP_ENTER: [Entry; 3] = {
    let [plain, modified] = kp(b"\x1bOM", b"\x1bO{}M");
    [plain, modified, any(b"\r")]
};

const BACKSPACE: [Entry; 29] = [
    // modifyOtherKeys not 2
    set(S, b"\x7f"),
    set(A, b"\x1b\x7f"),
    set(m!(A S), b"\x1b\x7f"),
    set(m!(C S), b"\x08"),
    set(m!(A C), b"\x1b\x08"),
    set(U, b"\x7f"),
    set(m!(U S), b"\x7f"),
    set(m!(A U), b"\x1b\x7f"),
    set(m!(A U S), b"\x1b\x7f"),
    set(m!(U C), b"\x08"),
    set(m!(U C S), b"\x08"),
    set(m!(A U C), b"\x1b\x08"),
    set(m!(A U C S), b"\x1b\x08"),
    // modifyOtherKeys 2
    set_other(S, b"\x1b[27;2;127~"),
    set_other(A, b"\x1b[27;3;127~"),
    set_other(m!(A S), b"\x1b[27;4;127~"),
    set_other(m!(C S), b"\x1b[27;6;127~"),
    set_other(m!(A C), b"\x1b[27;7;127~"),
    set_other(m!(A S C), b"\x1b[27;8;127~"),
    set_other(U, b"\x1b[27;9;127~"),
    set_other(m!(U S), b"\x1b[27;10;127~"),
    set_other(m!(A U), b"\x1b[27;11;127~"),
    set_other(m!(A U S), b"\x1b[27;12;127~"),
    set_other(m!(U C), b"\x1b[27;13;127~"),
    set_other(m!(U C S), b"\x1b[27;14;127~"),
    set_other(m!(A U C), b"\x1b[27;15;127~"),
    set_other(m!(A U C S), b"\x1b[27;16;127~"),
    Entry { decbkm: Some(b"\x7f"), ..exact(C, b"\x08") },
    Entry { decbkm: Some(b"\x08"), ..any(b"\x7f") },
];

const TAB: [Entry; 18] = [
    // modifyOtherKeys not 2
    set(S, b"\x1b[Z"),
    set(A, b"\x1b\t"),
    // modifyOtherKeys 2
    set_other(S, b"\x1b[27;2;9~"),
    set_other(A, b"\x1b[27;3;9~"),
    // Everything else
    exact(m!(A S), b"\x1b[27;4;9~"),
    exact(C, b"\x1b[27;5;9~"),
    exact(m!(C S), b"\x1b[27;6;9~"),
    exact(m!(A C), b"\x1b[27;7;9~"),
    exact(m!(A C S), b"\x1b[27;8;9~"),
    exact(U, b"\x1b[27;9;9~"),
    exact(m!(U S), b"\x1b[27;10;9~"),
    exact(m!(A U), b"\x1b[27;11;9~"),
    exact(m!(A U S), b"\x1b[27;12;9~"),
    exact(m!(U C), b"\x1b[27;13;9~"),
    exact(m!(U C S), b"\x1b[27;14;9~"),
    exact(m!(A U C), b"\x1b[27;15;9~"),
    exact(m!(A U C S), b"\x1b[27;16;9~"),
    any(b"\t"),
];

const ENTER: [Entry; 17] = [
    exact(S, b"\x1b[27;2;13~"),
    // modifyOtherKeys not 2
    set(A, b"\x1b\r"),
    // modifyOtherKeys 2
    set_other(A, b"\x1b[27;3;13~"),
    // Everything else
    exact(m!(A S), b"\x1b[27;4;13~"),
    exact(C, b"\x1b[27;5;13~"),
    exact(m!(C S), b"\x1b[27;6;13~"),
    exact(m!(A C), b"\x1b[27;7;13~"),
    exact(m!(A C S), b"\x1b[27;8;13~"),
    exact(U, b"\x1b[27;9;13~"),
    exact(m!(U S), b"\x1b[27;10;13~"),
    exact(m!(A U), b"\x1b[27;11;13~"),
    exact(m!(A U S), b"\x1b[27;12;13~"),
    exact(m!(U C), b"\x1b[27;13;13~"),
    exact(m!(U C S), b"\x1b[27;14;13~"),
    exact(m!(A U C), b"\x1b[27;15;13~"),
    exact(m!(A U C S), b"\x1b[27;16;13~"),
    any(b"\r"),
];

const ESCAPE: [Entry; 16] = [
    exact(S, b"\x1b[27;2;27~"),
    exact(A, b"\x1b\x1b"),
    exact(m!(A S), b"\x1b[27;4;27~"),
    exact(C, b"\x1b[27;5;27~"),
    exact(m!(C S), b"\x1b[27;6;27~"),
    exact(m!(A C), b"\x1b[27;7;27~"),
    exact(m!(A C S), b"\x1b[27;8;27~"),
    exact(U, b"\x1b[27;9;27~"),
    exact(m!(U S), b"\x1b[27;10;27~"),
    exact(m!(A U), b"\x1b[27;11;27~"),
    exact(m!(A U S), b"\x1b[27;12;27~"),
    exact(m!(U C), b"\x1b[27;13;27~"),
    exact(m!(U C S), b"\x1b[27;14;27~"),
    exact(m!(A U C), b"\x1b[27;15;27~"),
    exact(m!(A U C S), b"\x1b[27;16;27~"),
    any(b"\x1b"),
];

/// The entries for `key`, first match wins; most keys have none. ghostty `function_keys.zig:80`.
pub(crate) fn entries(key: Key) -> &'static [Entry] {
    match key {
        Key::ArrowUp | Key::NumpadUp => &ARROW_UP,
        Key::ArrowDown | Key::NumpadDown => &ARROW_DOWN,
        Key::ArrowRight | Key::NumpadRight => &ARROW_RIGHT,
        Key::ArrowLeft | Key::NumpadLeft => &ARROW_LEFT,
        Key::NumpadBegin => &BEGIN,
        Key::Home | Key::NumpadHome => &HOME,
        Key::End | Key::NumpadEnd => &END,
        Key::Insert | Key::NumpadInsert => &INSERT,
        Key::Delete | Key::NumpadDelete => &DELETE,
        Key::PageUp | Key::NumpadPageUp => &PAGE_UP,
        Key::PageDown | Key::NumpadPageDown => &PAGE_DOWN,
        Key::F1 => &F1,
        Key::F2 => &F2,
        Key::F3 => &F3,
        Key::F4 => &F4,
        Key::F5 => &F5,
        Key::F6 => &F6,
        Key::F7 => &F7,
        Key::F8 => &F8,
        Key::F9 => &F9,
        Key::F10 => &F10,
        Key::F11 => &F11,
        Key::F12 => &F12,
        Key::Numpad0 => &KP_0,
        Key::Numpad1 => &KP_1,
        Key::Numpad2 => &KP_2,
        Key::Numpad3 => &KP_3,
        Key::Numpad4 => &KP_4,
        Key::Numpad5 => &KP_5,
        Key::Numpad6 => &KP_6,
        Key::Numpad7 => &KP_7,
        Key::Numpad8 => &KP_8,
        Key::Numpad9 => &KP_9,
        Key::NumpadDecimal => &KP_DECIMAL,
        Key::NumpadDivide => &KP_DIVIDE,
        Key::NumpadMultiply => &KP_MULTIPLY,
        Key::NumpadSubtract => &KP_SUBTRACT,
        Key::NumpadAdd => &KP_ADD,
        Key::NumpadEnter => &KP_ENTER,
        Key::Backspace => &BACKSPACE,
        Key::Tab => &TAB,
        Key::Enter => &ENTER,
        Key::Escape => &ESCAPE,
        _ => &[],
    }
}

/// Every key with entries, for the table's own tests.
#[cfg(test)]
pub(crate) const KEYS_WITH_ENTRIES: &[Key] = &[
    Key::ArrowUp,
    Key::ArrowDown,
    Key::ArrowRight,
    Key::ArrowLeft,
    Key::Home,
    Key::End,
    Key::Insert,
    Key::Delete,
    Key::PageUp,
    Key::PageDown,
    Key::F1,
    Key::F2,
    Key::F3,
    Key::F4,
    Key::F5,
    Key::F6,
    Key::F7,
    Key::F8,
    Key::F9,
    Key::F10,
    Key::F11,
    Key::F12,
    Key::Numpad0,
    Key::Numpad1,
    Key::Numpad2,
    Key::Numpad3,
    Key::Numpad4,
    Key::Numpad5,
    Key::Numpad6,
    Key::Numpad7,
    Key::Numpad8,
    Key::Numpad9,
    Key::NumpadDecimal,
    Key::NumpadDivide,
    Key::NumpadMultiply,
    Key::NumpadSubtract,
    Key::NumpadAdd,
    Key::NumpadEnter,
    Key::NumpadUp,
    Key::NumpadDown,
    Key::NumpadRight,
    Key::NumpadLeft,
    Key::NumpadBegin,
    Key::NumpadHome,
    Key::NumpadEnd,
    Key::NumpadInsert,
    Key::NumpadDelete,
    Key::NumpadPageUp,
    Key::NumpadPageDown,
    Key::Backspace,
    Key::Tab,
    Key::Enter,
    Key::Escape,
];
