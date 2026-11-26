//! AppKit `NSEvent` mapping to `blaze-vt`'s [`KeyEvent`]: physical macOS virtual key code,
//! modifiers, and composed UTF-8 text.

use blaze_vt::input::key::{Action, Key, KeyEvent, Mods};
use objc2_app_kit::{NSEvent, NSEventModifierFlags};

pub fn key_of(code: u16) -> Key {
    use Key as K;
    match code {
        0x00 => K::KeyA,
        0x01 => K::KeyS,
        0x02 => K::KeyD,
        0x03 => K::KeyF,
        0x04 => K::KeyH,
        0x05 => K::KeyG,
        0x06 => K::KeyZ,
        0x07 => K::KeyX,
        0x08 => K::KeyC,
        0x09 => K::KeyV,
        0x0B => K::KeyB,
        0x0C => K::KeyQ,
        0x0D => K::KeyW,
        0x0E => K::KeyE,
        0x0F => K::KeyR,
        0x10 => K::KeyY,
        0x11 => K::KeyT,
        0x12 => K::Digit1,
        0x13 => K::Digit2,
        0x14 => K::Digit3,
        0x15 => K::Digit4,
        0x16 => K::Digit6,
        0x17 => K::Digit5,
        0x18 => K::Equal,
        0x19 => K::Digit9,
        0x1A => K::Digit7,
        0x1B => K::Minus,
        0x1C => K::Digit8,
        0x1D => K::Digit0,
        0x1E => K::BracketRight,
        0x1F => K::KeyO,
        0x20 => K::KeyU,
        0x21 => K::BracketLeft,
        0x22 => K::KeyI,
        0x23 => K::KeyP,
        0x24 => K::Enter,
        0x25 => K::KeyL,
        0x26 => K::KeyJ,
        0x27 => K::Quote,
        0x28 => K::KeyK,
        0x29 => K::Semicolon,
        0x2A => K::Backslash,
        0x2B => K::Comma,
        0x2C => K::Slash,
        0x2D => K::KeyN,
        0x2E => K::KeyM,
        0x2F => K::Period,
        0x30 => K::Tab,
        0x31 => K::Space,
        0x32 => K::Backquote,
        0x33 => K::Backspace,
        0x35 => K::Escape,
        0x4C => K::NumpadEnter,
        0x60 => K::F5,
        0x61 => K::F6,
        0x62 => K::F7,
        0x63 => K::F3,
        0x64 => K::F8,
        0x65 => K::F9,
        0x67 => K::F11,
        0x6D => K::F10,
        0x6F => K::F12,
        0x73 => K::Home,
        0x74 => K::PageUp,
        0x75 => K::Delete,
        0x76 => K::F4,
        0x77 => K::End,
        0x78 => K::F2,
        0x79 => K::PageDown,
        0x7A => K::F1,
        0x7B => K::ArrowLeft,
        0x7C => K::ArrowRight,
        0x7D => K::ArrowDown,
        0x7E => K::ArrowUp,
        _ => K::Unidentified,
    }
}

pub fn mods_of(flags: NSEventModifierFlags) -> Mods {
    let mut m = Mods::NONE;
    if flags.contains(NSEventModifierFlags::Shift) {
        m = m | Mods::SHIFT;
    }
    if flags.contains(NSEventModifierFlags::Control) {
        m = m | Mods::CTRL;
    }
    if flags.contains(NSEventModifierFlags::Option) {
        m = m | Mods::ALT;
    }
    if flags.contains(NSEventModifierFlags::Command) {
        m = m | Mods::SUPER;
    }
    if flags.contains(NSEventModifierFlags::CapsLock) {
        m = m | Mods::CAPS_LOCK;
    }
    m
}

/// Composed printable text carried by `ev` (empty for control characters and AppKit private-use
/// function keys).
pub fn text_of(ev: &NSEvent) -> String {
    let s = ev.characters().map(|s| s.to_string()).unwrap_or_default();
    if s.chars()
        .any(|c| (c as u32) < 0x20 || c == '\u{7f}' || ('\u{F700}'..='\u{F8FF}').contains(&c))
    {
        return String::new();
    }
    s
}

pub fn unshifted_of(ev: &NSEvent) -> Option<char> {
    let s = ev.charactersIgnoringModifiers()?.to_string();
    let c = s.chars().next()?;
    if (c as u32) < 0x20 || ('\u{F700}'..='\u{F8FF}').contains(&c) {
        None
    } else {
        Some(c.to_ascii_lowercase())
    }
}

pub fn event<'a>(ev: &NSEvent, text: &'a str) -> KeyEvent<'a> {
    let mods = mods_of(ev.modifierFlags());
    let mut consumed = Mods::NONE;
    if !text.is_empty() {
        for m in [Mods::SHIFT, Mods::ALT] {
            if mods.contains(m) {
                consumed = consumed | m;
            }
        }
    }
    KeyEvent {
        action: if ev.isARepeat() { Action::Repeat } else { Action::Press },
        key: key_of(ev.keyCode()),
        mods,
        consumed_mods: consumed,
        composing: false,
        utf8: text.as_bytes(),
        unshifted_codepoint: unshifted_of(ev),
    }
}
