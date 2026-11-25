//! Key encoding, a port of ghostty's `src/input/key_encode.zig` (with `key.zig`'s `Key` and
//! `key_mods.zig`'s `Mods`).
//!
//! [`encode`] turns one [`KeyEvent`] into the bytes for the PTY under the [`EncodeOpts`] the
//! terminal's modes give. With any kitty keyboard flag set it is the kitty protocol
//! (<https://sw.kovidgoyal.net/kitty/keyboard-protocol/>); otherwise it is "legacy": xterm's
//! PC-style function keys, C0 controls for ctrl, the alt-ESC prefix, xterm's modifyOtherKeys
//! mode 2 and the fixterms `CSI u` form for other ctrl chords, which together never change what a
//! plain terminal sends.
//!
//! What ghostty decides at compile time from the target OS (`builtin.os.tag == .macos`:
//! option-as-alt, Cmd+key sending no text) is [`EncodeOpts::platform`] here, so one build
//! encodes for either and the tests run both.

use super::function_keys::{self, CursorMode, KeypadMode, ModifyKeys, ModsMatch};
use super::kitty_keys;
use super::{BufferFull, Out};

#[cfg(test)]
#[path = "key_tests.rs"]
mod tests;

/// A physical key, by its W3C `KeyboardEvent.code` (<https://www.w3.org/TR/uievents-code>),
/// independent of layout: the key a US layout calls "a" is [`Key::KeyA`] on a Russian layout
/// too. The layout's meaning arrives as [`KeyEvent::utf8`] and
/// [`KeyEvent::unshifted_codepoint`]. ghostty `key.zig:106`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Key {
    #[default]
    Unidentified,

    // "Writing System Keys" § 3.1.1
    Backquote,
    Backslash,
    BracketLeft,
    BracketRight,
    Comma,
    Digit0,
    Digit1,
    Digit2,
    Digit3,
    Digit4,
    Digit5,
    Digit6,
    Digit7,
    Digit8,
    Digit9,
    Equal,
    IntlBackslash,
    IntlRo,
    IntlYen,
    KeyA,
    KeyB,
    KeyC,
    KeyD,
    KeyE,
    KeyF,
    KeyG,
    KeyH,
    KeyI,
    KeyJ,
    KeyK,
    KeyL,
    KeyM,
    KeyN,
    KeyO,
    KeyP,
    KeyQ,
    KeyR,
    KeyS,
    KeyT,
    KeyU,
    KeyV,
    KeyW,
    KeyX,
    KeyY,
    KeyZ,
    Minus,
    Period,
    Quote,
    Semicolon,
    Slash,

    // "Functional Keys" § 3.1.2
    AltLeft,
    AltRight,
    Backspace,
    CapsLock,
    ContextMenu,
    ControlLeft,
    ControlRight,
    Enter,
    MetaLeft,
    MetaRight,
    ShiftLeft,
    ShiftRight,
    Space,
    Tab,
    Convert,
    KanaMode,
    NonConvert,

    // "Control Pad Section" § 3.2
    Delete,
    End,
    Help,
    Home,
    Insert,
    PageDown,
    PageUp,

    // "Arrow Pad Section" § 3.3
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    ArrowUp,

    // "Numpad Section" § 3.4
    NumLock,
    Numpad0,
    Numpad1,
    Numpad2,
    Numpad3,
    Numpad4,
    Numpad5,
    Numpad6,
    Numpad7,
    Numpad8,
    Numpad9,
    NumpadAdd,
    NumpadBackspace,
    NumpadClear,
    NumpadClearEntry,
    NumpadComma,
    NumpadDecimal,
    NumpadDivide,
    NumpadEnter,
    NumpadEqual,
    NumpadMemoryAdd,
    NumpadMemoryClear,
    NumpadMemoryRecall,
    NumpadMemoryStore,
    NumpadMemorySubtract,
    NumpadMultiply,
    NumpadParenLeft,
    NumpadParenRight,
    NumpadSubtract,
    // Keypad keys the legacy and kitty encodings tell apart (ghostty `key.zig:226`).
    NumpadSeparator,
    NumpadUp,
    NumpadDown,
    NumpadRight,
    NumpadLeft,
    NumpadBegin,
    NumpadHome,
    NumpadEnd,
    NumpadInsert,
    NumpadDelete,
    NumpadPageUp,
    NumpadPageDown,

    // "Function Section" § 3.5
    Escape,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    F13,
    F14,
    F15,
    F16,
    F17,
    F18,
    F19,
    F20,
    F21,
    F22,
    F23,
    F24,
    F25,
    FnLock,
    PrintScreen,
    ScrollLock,
    Pause,

    // "Media Keys" § 3.6
    BrowserBack,
    BrowserFavorites,
    BrowserForward,
    BrowserHome,
    BrowserRefresh,
    BrowserSearch,
    BrowserStop,
    Eject,
    LaunchApp1,
    LaunchApp2,
    LaunchMail,
    MediaPlayPause,
    MediaSelect,
    MediaStop,
    MediaTrackNext,
    MediaTrackPrevious,
    Power,
    Sleep,
    AudioVolumeDown,
    AudioVolumeMute,
    AudioVolumeUp,
    WakeUp,

    // "Legacy, Non-standard, and Special Keys" § 3.7
    Copy,
    Cut,
    Paste,
}

impl Key {
    /// The character this key has on a US layout unshifted, if it is printable (ghostty
    /// `key.zig:529`, from its `codepoint_map` at `key.zig:779`). Tab is `\t`; keypad digits and
    /// operators have theirs.
    pub const fn codepoint(self) -> Option<char> {
        use Key::*;
        Some(match self {
            KeyA => 'a',
            KeyB => 'b',
            KeyC => 'c',
            KeyD => 'd',
            KeyE => 'e',
            KeyF => 'f',
            KeyG => 'g',
            KeyH => 'h',
            KeyI => 'i',
            KeyJ => 'j',
            KeyK => 'k',
            KeyL => 'l',
            KeyM => 'm',
            KeyN => 'n',
            KeyO => 'o',
            KeyP => 'p',
            KeyQ => 'q',
            KeyR => 'r',
            KeyS => 's',
            KeyT => 't',
            KeyU => 'u',
            KeyV => 'v',
            KeyW => 'w',
            KeyX => 'x',
            KeyY => 'y',
            KeyZ => 'z',
            Digit0 => '0',
            Digit1 => '1',
            Digit2 => '2',
            Digit3 => '3',
            Digit4 => '4',
            Digit5 => '5',
            Digit6 => '6',
            Digit7 => '7',
            Digit8 => '8',
            Digit9 => '9',
            Semicolon => ';',
            Space => ' ',
            Quote => '\'',
            Comma => ',',
            Backquote => '`',
            Period => '.',
            Slash => '/',
            Minus => '-',
            Equal => '=',
            BracketLeft => '[',
            BracketRight => ']',
            Backslash => '\\',
            Tab => '\t',
            Numpad0 => '0',
            Numpad1 => '1',
            Numpad2 => '2',
            Numpad3 => '3',
            Numpad4 => '4',
            Numpad5 => '5',
            Numpad6 => '6',
            Numpad7 => '7',
            Numpad8 => '8',
            Numpad9 => '9',
            NumpadDecimal => '.',
            NumpadDivide => '/',
            NumpadMultiply => '*',
            NumpadSubtract => '-',
            NumpadAdd => '+',
            NumpadEqual => '=',
            _ => return None,
        })
    }

    /// Whether this is a modifier key (shift, control, alt, meta; either side). ghostty
    /// `key.zig:422`.
    pub const fn is_modifier(self) -> bool {
        matches!(
            self,
            Key::ShiftLeft
                | Key::ControlLeft
                | Key::AltLeft
                | Key::MetaLeft
                | Key::ShiftRight
                | Key::ControlRight
                | Key::AltRight
                | Key::MetaRight
        )
    }
}

/// Which side of the keyboard a held modifier is on.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Side {
    #[default]
    Left,
    Right,
}

/// The modifiers held, ghostty's `Mods` (`key_mods.zig:29`) bit for bit: shift, ctrl, alt,
/// super, caps lock, num lock, then one side bit per modifier (set: the right one). A side bit
/// means something only with its modifier.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Mods(u16);

impl Mods {
    pub const NONE: Mods = Mods(0);
    pub const SHIFT: Mods = Mods(1 << 0);
    pub const CTRL: Mods = Mods(1 << 1);
    pub const ALT: Mods = Mods(1 << 2);
    /// Super: Cmd on macOS, the logo key elsewhere.
    pub const SUPER: Mods = Mods(1 << 3);
    pub const CAPS_LOCK: Mods = Mods(1 << 4);
    pub const NUM_LOCK: Mods = Mods(1 << 5);
    /// Shift is the right shift.
    pub const SHIFT_RIGHT_SIDE: Mods = Mods(1 << 6);
    /// Ctrl is the right ctrl.
    pub const CTRL_RIGHT_SIDE: Mods = Mods(1 << 7);
    /// Alt is the right alt (on macOS, the right option).
    pub const ALT_RIGHT_SIDE: Mods = Mods(1 << 8);
    /// Super is the right super.
    pub const SUPER_RIGHT_SIDE: Mods = Mods(1 << 9);

    const ALL: u16 = (1 << 10) - 1;

    /// From ghostty's bit layout; bits past the side bits are dropped.
    pub const fn from_bits(bits: u16) -> Mods {
        Mods(bits & Mods::ALL)
    }

    pub const fn bits(self) -> u16 {
        self.0
    }

    pub const fn union(self, other: Mods) -> Mods {
        Mods(self.0 | other.0)
    }

    /// Whether every bit of `other` is set.
    pub const fn contains(self, other: Mods) -> bool {
        self.0 & other.0 == other.0
    }

    /// `self & !other` (ghostty `unset`).
    pub const fn without(self, other: Mods) -> Mods {
        Mods(self.0 & !other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Only shift, ctrl, alt and super: no locks, no sides (ghostty `binding`).
    pub const fn binding(self) -> Mods {
        Mods(self.0 & 0b1111)
    }

    pub const fn shift(self) -> bool {
        self.contains(Mods::SHIFT)
    }

    pub const fn ctrl(self) -> bool {
        self.contains(Mods::CTRL)
    }

    pub const fn alt(self) -> bool {
        self.contains(Mods::ALT)
    }

    pub const fn super_key(self) -> bool {
        self.contains(Mods::SUPER)
    }

    pub const fn caps_lock(self) -> bool {
        self.contains(Mods::CAPS_LOCK)
    }

    pub const fn num_lock(self) -> bool {
        self.contains(Mods::NUM_LOCK)
    }

    /// The side alt is held on.
    pub const fn alt_side(self) -> Side {
        if self.contains(Mods::ALT_RIGHT_SIDE) { Side::Right } else { Side::Left }
    }

    /// The legacy modifier parameter, `1 + (shift | alt << 1 | ctrl << 2 | super << 3)`, for a
    /// non-empty set of binding modifiers: 2 S, 3 A, 4 SA, 5 C, … 16 SACU (ghostty
    /// `function_keys.zig:53`, whose `modifiers` table is index + 2).
    pub(crate) const fn legacy_param(self) -> Option<u8> {
        let b = self.binding();
        if b.is_empty() {
            return None;
        }
        let bits = (b.shift() as u8)
            | (b.alt() as u8) << 1
            | (b.ctrl() as u8) << 2
            | (b.super_key() as u8) << 3;
        Some(bits + 1)
    }
}

impl core::ops::BitOr for Mods {
    type Output = Mods;
    fn bitor(self, rhs: Mods) -> Mods {
        self.union(rhs)
    }
}

impl core::ops::BitOrAssign for Mods {
    fn bitor_assign(&mut self, rhs: Mods) {
        *self = self.union(rhs);
    }
}

/// What the key did.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Action {
    Release,
    #[default]
    Press,
    Repeat,
}

/// One key event as the host's input layer reports it (ghostty `key.zig:16`). Hosts that cannot
/// tell some field leave its default and get the closest encoding.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct KeyEvent<'a> {
    pub action: Action,
    /// The physical key, layout-independent.
    pub key: Key,
    /// Every modifier held, locks and sides included.
    pub mods: Mods,
    /// The modifiers the layout used up to produce `utf8` (shift for `A`, option for `∑`).
    /// Meaningless when `utf8` is empty.
    pub consumed_mods: Mods,
    /// Mid dead-key or IME composition; `utf8`, if any, is preedit text.
    pub composing: bool,
    /// The text the layout produced, the logical key. Should be UTF-8; invalid bytes are
    /// handled as ghostty does (as if there were no code point there).
    pub utf8: &'a [u8],
    /// The character the key gives without shift on the current layout (`a` for shift+a).
    pub unshifted_codepoint: Option<char>,
}

impl KeyEvent<'_> {
    /// The modifiers that count for bindings: all of them, less the consumed ones when there is
    /// text (ghostty `key.zig:52`).
    pub const fn effective_mods(&self) -> Mods {
        if self.utf8.is_empty() { self.mods } else { self.mods.without(self.consumed_mods) }
    }
}

/// The kitty keyboard protocol flags (`CSI > flags u`), ghostty `terminal/kitty/key.zig:79`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct KittyFlags(u8);

impl KittyFlags {
    pub const DISABLED: KittyFlags = KittyFlags(0);
    /// 1: disambiguate escape codes.
    pub const DISAMBIGUATE: KittyFlags = KittyFlags(1);
    /// 2: report repeat and release events.
    pub const REPORT_EVENTS: KittyFlags = KittyFlags(2);
    /// 4: report shifted and base-layout alternate keys.
    pub const REPORT_ALTERNATES: KittyFlags = KittyFlags(4);
    /// 8: report all keys as escape codes.
    pub const REPORT_ALL: KittyFlags = KittyFlags(8);
    /// 16: report associated text.
    pub const REPORT_ASSOCIATED: KittyFlags = KittyFlags(16);
    /// Every flag.
    pub const ALL: KittyFlags = KittyFlags(31);

    /// From the protocol's integer; bits above the five flags are dropped.
    pub const fn from_bits(bits: u8) -> KittyFlags {
        KittyFlags(bits & 31)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn union(self, other: KittyFlags) -> KittyFlags {
        KittyFlags(self.0 | other.0)
    }

    pub const fn contains(self, other: KittyFlags) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl core::ops::BitOr for KittyFlags {
    type Output = KittyFlags;
    fn bitor(self, rhs: KittyFlags) -> KittyFlags {
        self.union(rhs)
    }
}

/// ghostty's `macos-option-as-alt`: which option keys act as alt (`input/config.zig:3`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum OptionAsAlt {
    /// Option composes text (`∑` for option+w), as macOS does.
    #[default]
    False,
    True,
    Left,
    Right,
}

/// The OS whose keyboard conventions apply: ghostty decides these at compile time.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Platform {
    /// Option-as-alt applies, and Cmd (super) + key sends no text.
    MacOs,
    /// Linux, the BSDs and the rest: alt is always alt, super + key sends its text.
    Other,
}

/// Everything that changes how a key encodes: terminal modes the core holds, plus the host's
/// platform and option-as-alt setting (ghostty `key_encode.zig:14`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct EncodeOpts {
    pub platform: Platform,
    /// DECCKM (DEC mode 1).
    pub cursor_key_application: bool,
    /// DECKPAM / DECNKM (DEC mode 66).
    pub keypad_key_application: bool,
    /// DECBKM (DEC mode 67): backspace sends BS, ctrl+backspace DEL.
    pub backarrow_key_mode: bool,
    /// DEC mode 1035: keypad keys are numeric whatever 66 says.
    pub ignore_keypad_with_numlock: bool,
    /// DEC mode 1036: alt prefixes ESC.
    pub alt_esc_prefix: bool,
    /// xterm modifyOtherKeys mode 2 (`CSI > 4 ; 2 m`).
    pub modify_other_keys_state_2: bool,
    /// The kitty keyboard flags on top of the active screen's stack.
    pub kitty_flags: KittyFlags,
    pub macos_option_as_alt: OptionAsAlt,
}

impl EncodeOpts {
    /// Every mode off (a terminal after RIS), for `platform`.
    pub const fn new(platform: Platform) -> EncodeOpts {
        EncodeOpts {
            platform,
            cursor_key_application: false,
            keypad_key_application: false,
            backarrow_key_mode: false,
            ignore_keypad_with_numlock: false,
            alt_esc_prefix: false,
            modify_other_keys_state_2: false,
            kitty_flags: KittyFlags::DISABLED,
            macos_option_as_alt: OptionAsAlt::False,
        }
    }
}

/// The most bytes [`encode`] writes for `event`: the longest fixed sequence plus four bytes per
/// byte of text (a kitty associated-text code point is at most four bytes per UTF-8 byte).
pub const fn max_len(event: &KeyEvent<'_>) -> usize {
    MAX_FIXED_LEN + 4 * event.utf8.len()
}

/// `ESC [` code `:` shifted `:` base `;` mods `:` event `;;` final, every number at its widest.
const MAX_FIXED_LEN: usize = 2 + 7 + 8 + 8 + 4 + 2 + 2 + 1;

/// Encodes `event` into `out` and returns the bytes written, which may be none (a release in
/// legacy mode, a dead key, a lone modifier).
pub fn encode<'o>(
    event: &KeyEvent<'_>,
    opts: &EncodeOpts,
    out: &'o mut [u8],
) -> Result<&'o [u8], BufferFull> {
    let mut w = Out::new(out);
    if opts.kitty_flags.is_empty() {
        legacy(&mut w, event, opts)?;
    } else {
        kitty(&mut w, event, opts)?;
    }
    Ok(w.finish())
}

/// ghostty `key_encode.zig:99`.
fn kitty(w: &mut Out<'_>, event: &KeyEvent<'_>, opts: &EncodeOpts) -> Result<(), BufferFull> {
    let flags = opts.kitty_flags;
    if flags.is_empty() {
        return legacy(w, event, opts);
    }

    // Only presses, unless events are reported.
    if event.action == Action::Release {
        if !flags.contains(KittyFlags::REPORT_EVENTS) {
            return Ok(());
        }
        // Enter, backspace and tab report releases only with "report all".
        if !flags.contains(KittyFlags::REPORT_ALL)
            && matches!(event.key, Key::Enter | Key::Backspace | Key::Tab)
        {
            return Ok(());
        }
    }

    let all_mods = event.mods;
    let binding_mods = event.effective_mods().binding();

    // The functional key table first; otherwise the unshifted code point.
    let entry = kitty_keys::entry(event.key).or_else(|| {
        event.unshifted_codepoint.map(|cp| kitty_keys::Entry {
            code: u32::from(cp),
            final_byte: b'u',
            modifier: false,
        })
    });

    'preprocessing: {
        // While composing, only plain modifiers are sent.
        if event.composing {
            if entry.is_some_and(|e| e.modifier) {
                break 'preprocessing;
            }
            return Ok(());
        }

        // IME confirmation still sends an enter key: enter (or backspace) with text that is not
        // a single control byte is that text (backspace: nothing).
        if !event.utf8.is_empty()
            && matches!(event.key, Key::Enter | Key::Backspace)
            && !is_control_utf8(event.utf8)
        {
            if event.key == Key::Backspace {
                return Ok(());
            }
            return w.bytes(event.utf8);
        }

        if !flags.contains(KittyFlags::REPORT_ALL) {
            // Enter, tab and backspace stay legacy so a shell is usable after a program that set
            // the mode crashes.
            if binding_mods.is_empty() {
                match event.key {
                    Key::Enter => return w.byte(b'\r'),
                    Key::Tab => return w.byte(b'\t'),
                    Key::Backspace => return w.byte(0x7F),
                    _ => {}
                }
            }

            // Unmodified printable text goes as text (not releases: those are encoded).
            if !event.utf8.is_empty() && binding_mods.is_empty() && event.action != Action::Release
            {
                // Invalid UTF-8 is encoded as if the key had no text.
                if let Ok(text) = core::str::from_utf8(event.utf8)
                    && !text.chars().any(|c| is_control(u32::from(c)))
                {
                    return w.bytes(event.utf8);
                }
            }
        }
    }

    let Some(entry) = entry else {
        // No key to name: pure text (composed or IME) goes as is.
        if !event.utf8.is_empty() {
            return w.bytes(event.utf8);
        }
        return Ok(());
    };

    // A lone modifier needs "report all".
    if entry.modifier && !flags.contains(KittyFlags::REPORT_ALL) {
        return Ok(());
    }

    let mut seq = KittySequence {
        key: entry.code,
        final_byte: entry.final_byte,
        mods: KittyMods::from_input(all_mods),
        event: KittyEvent::None,
        alternates: [None, None],
        text: b"",
    };

    if flags.contains(KittyFlags::REPORT_EVENTS) {
        seq.event = match event.action {
            Action::Press => KittyEvent::Press,
            Action::Release => KittyEvent::Release,
            Action::Repeat => KittyEvent::Repeat,
        };
    }

    if flags.contains(KittyFlags::REPORT_ALTERNATES) && !is_control(seq.key) {
        // Invalid UTF-8 counts as no alternates at all.
        if let Ok(text) = core::str::from_utf8(event.utf8) {
            let mut chars = text.chars();
            let base = event.key.codepoint().map(u32::from);
            if let Some(cp1) = chars.next() {
                let cp1 = u32::from(cp1);
                // The shifted key.
                if cp1 != seq.key && seq.mods.shift() {
                    seq.alternates[0] = Some(cp1);
                }
                // The base-layout key, when it differs from the key and the text is that one
                // code point not equal to it.
                let has_cp2 = chars.next().is_some();
                if let Some(base) = base
                    && base != seq.key
                    && cp1 != base
                    && !has_cp2
                {
                    seq.alternates[1] = Some(base);
                }
            } else if let Some(base) = base
                && base != seq.key
            {
                seq.alternates[1] = Some(base);
            }
        }
    }

    if flags.contains(KittyFlags::REPORT_ASSOCIATED) && seq.event != KittyEvent::Release {
        // On macOS alt is option and composes text unless option-as-alt claims that side.
        let alt_prevents_text = match opts.platform {
            Platform::MacOs => match opts.macos_option_as_alt {
                OptionAsAlt::Left => all_mods.alt_side() == Side::Left,
                OptionAsAlt::Right => all_mods.alt_side() == Side::Right,
                OptionAsAlt::True => true,
                OptionAsAlt::False => false,
            },
            Platform::Other => true,
        };
        if !seq.mods.prevents_text(alt_prevents_text) {
            seq.text = event.utf8;
        }
    }

    seq.encode(w)
}

/// ghostty `key_encode.zig:328`.
fn legacy(w: &mut Out<'_>, event: &KeyEvent<'_>, opts: &EncodeOpts) -> Result<(), BufferFull> {
    let all_mods = event.mods;
    let binding_mods = event.effective_mods().binding();

    // Only presses and repeats; nothing mid dead key.
    if !matches!(event.action, Action::Press | Action::Repeat) || event.composing {
        return Ok(());
    }

    // A PC-style function key is its table sequence.
    if let Some(seq) = pc_style_function_key(event.key, all_mods, opts) {
        // With text, backspace/enter/escape belong to the IME (escape clears a Japanese dead key,
        // commits a Korean one; backspace deletes one preedit char), unless the text is a lone
        // control byte, which macOS sends for these keys.
        let ime = !event.utf8.is_empty()
            && matches!(event.key, Key::Backspace | Key::Enter | Key::Escape)
            && !is_control_utf8(event.utf8);
        if ime {
            if event.key == Key::Backspace {
                return Ok(());
            }
        } else {
            return seq.write(w);
        }
    }

    // ctrl+char as a C0 byte, alt as an ESC prefix.
    if let Some(c0) = ctrl_seq(event.key, event.utf8, event.unshifted_codepoint, all_mods) {
        if binding_mods.alt() {
            w.byte(0x1B)?;
        }
        return w.byte(c0);
    }

    // No text: only alt-prefixing the unshifted code point remains.
    let utf8 = event.utf8;
    if utf8.is_empty() {
        if let Some(b) = legacy_alt_prefix(event, binding_mods, all_mods, opts) {
            w.byte(0x1B)?;
            w.byte(b)?;
        }
        return Ok(());
    }

    // modifyOtherKeys 2: `CSI 27 ; mods ; cp ~` for a modified single code point.
    if opts.modify_other_keys_state_2
        && let Ok(text) = core::str::from_utf8(utf8)
    {
        let mut chars = text.chars();
        if let (Some(cp), None) = (chars.next(), chars.next()) {
            let cp = u32::from(cp);
            // Binding mods, less alt when it is macOS option composing text.
            let mut mods = event.mods.binding();
            if opts.platform == Platform::MacOs {
                let alt_is_alt = match opts.macos_option_as_alt {
                    OptionAsAlt::False => false,
                    OptionAsAlt::True => true,
                    OptionAsAlt::Left => event.mods.alt_side() == Side::Left,
                    OptionAsAlt::Right => event.mods.alt_side() == Side::Right,
                };
                if !alt_is_alt {
                    mods = mods.without(Mods::ALT);
                }
            }
            // xterm's `ModifyOtherKeys`: control input, any non-shift modifier, or shift+space.
            let should_modify = (0x40..=0x7F).contains(&cp)
                || !mods.without(Mods::SHIFT).is_empty()
                || cp == u32::from(' ');
            if should_modify && let Some(param) = mods.legacy_param() {
                w.bytes(b"\x1B[27;")?;
                w.dec(u32::from(param))?;
                w.byte(b';')?;
                w.dec(cp)?;
                return w.byte(b'~');
            }
        }
    }

    // fixterms `CSI cp ; mods u` for ctrl with a single code point, with all the mods (kitty's
    // choice, not the effective ones).
    if event.mods.ctrl()
        && let Ok(text) = core::str::from_utf8(event.utf8)
    {
        let mut chars = text.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            let mut mods = CsiUMods::from_input(event.mods);
            let mut c = c;
            // Kitty, not fixterms: shifted A–Z are sent lowercase with shift.
            if c.is_ascii_uppercase() && mods.shift {
                c = c.to_ascii_lowercase();
            }
            // Shift counts only when it did not produce the character (fixterms).
            if event.unshifted_codepoint != Some(c) {
                mods.shift = false;
            }
            w.bytes(b"\x1B[")?;
            w.dec(u32::from(c))?;
            w.byte(b';')?;
            w.dec(u32::from(mods.seq_int()))?;
            return w.byte(b'u');
        }
    }

    if let Some(b) = legacy_alt_prefix(event, binding_mods, all_mods, opts) {
        w.byte(0x1B)?;
        return w.byte(b);
    }

    // macOS: Cmd+key sends no text (TextEdit, Terminal.app and iTerm2 agree). Elsewhere super+b
    // sends "b", as GNOME Console does.
    if opts.platform == Platform::MacOs && all_mods.super_key() {
        return Ok(());
    }

    w.bytes(utf8)
}

/// The byte alt prefixes with ESC, if alt is pressed, 1036 is set and (on macOS) option is
/// alt on that side. ghostty `key_encode.zig:551`.
fn legacy_alt_prefix(
    event: &KeyEvent<'_>,
    binding_mods: Mods,
    mods: Mods,
    opts: &EncodeOpts,
) -> Option<u8> {
    if !binding_mods.alt() || !opts.alt_esc_prefix {
        return None;
    }
    if opts.platform == Platform::MacOs {
        match opts.macos_option_as_alt {
            OptionAsAlt::False => return None,
            OptionAsAlt::Left if mods.alt_side() == Side::Right => return None,
            OptionAsAlt::Right if mods.alt_side() == Side::Left => return None,
            _ => {}
        }
    }
    // The one byte of text, else an unshifted code point below 256.
    if let [b] = event.utf8 {
        return Some(*b);
    }
    event.unshifted_codepoint.and_then(|c| u8::try_from(u32::from(c)).ok())
}

/// The legacy table sequence for `key` under these modes, if any. ghostty
/// `key_encode.zig:604`.
fn pc_style_function_key(
    key: Key,
    mods: Mods,
    opts: &EncodeOpts,
) -> Option<function_keys::Sequence> {
    // Sides and locks do not matter here.
    let mods = mods.binding();
    // Mode 1035 forces the numeric keypad; otherwise 66 decides.
    let keypad_application = !opts.ignore_keypad_with_numlock && opts.keypad_key_application;
    let cursor_application = opts.cursor_key_application;
    let mok2 = opts.modify_other_keys_state_2;

    for entry in function_keys::entries(key) {
        match entry.cursor {
            CursorMode::Any => {}
            CursorMode::Normal if cursor_application => continue,
            CursorMode::Application if !cursor_application => continue,
            _ => {}
        }
        if entry.keypad == KeypadMode::Application && !keypad_application {
            continue;
        }
        match entry.modify_other_keys {
            ModifyKeys::Any => {}
            ModifyKeys::Set if mok2 => continue,
            ModifyKeys::SetOther if !mok2 => continue,
            _ => {}
        }
        let param = match entry.mods {
            ModsMatch::Any => None,
            ModsMatch::None if !mods.is_empty() => continue,
            ModsMatch::None => None,
            ModsMatch::Exact(m) if m != mods => continue,
            ModsMatch::Exact(_) => None,
            // The fifteen pcStyle entries, one per modifier set: any non-empty set, its param.
            ModsMatch::EachModifier => match mods.legacy_param() {
                Some(p) => Some(p),
                None => continue,
            },
        };
        let bytes = match (opts.backarrow_key_mode, entry.decbkm) {
            (true, Some(decbkm)) => decbkm,
            _ => entry.sequence,
        };
        return Some(function_keys::Sequence { bytes, param });
    }
    None
}

/// The C0 byte ctrl+key sends, if it sends one: Kitty's table, with fixterms' `i`, `m` and `[`
/// left to `CSI u`. ghostty `key_encode.zig:684`.
fn ctrl_seq(
    logical_key: Key,
    utf8: &[u8],
    unshifted_codepoint: Option<char>,
    mods: Mods,
) -> Option<u8> {
    if !mods.ctrl() {
        return None;
    }
    // Binding mods without alt: alt is the ESC prefix, handled by the caller.
    let mut unset = mods.binding().without(Mods::ALT);

    let mut c = if let [b] = utf8 {
        // One byte of text is the character.
        *b
    } else if let Some(cp) = logical_key.codepoint() {
        // The key's US character, for layouts whose `c` key types `с` (Russian, Mongolian),
        // which every terminal sends as ctrl+c. Only with ctrl alone: shift needs the layout.
        if unset != Mods::CTRL {
            return None;
        }
        cp as u8
    } else {
        return None;
    };

    // Shift outside A–Z (but `@`) is part of the character: ctrl+shift+- is ctrl+_ for emacs.
    if unset.shift() && !c.is_ascii_uppercase() && c != b'@' {
        unset = unset.without(Mods::SHIFT);
    }

    // Uppercase from caps lock: the unshifted code point gives the letter. (ctrl+shift+m is not
    // a ctrl-seq; it falls through to CSI u so programs can tell it from ctrl+m.)
    if c.is_ascii_uppercase()
        && let Some(u) = unshifted_codepoint
        && let Ok(b) = u8::try_from(u32::from(u))
    {
        c = b;
    }

    if unset != Mods::CTRL {
        return None;
    }

    Some(match c {
        b' ' => 0,
        b'/' => 31,
        b'0' => 48,
        b'1' => 49,
        b'2' => 0,
        b'3' => 27,
        b'4' => 28,
        b'5' => 29,
        b'6' => 30,
        b'7' => 31,
        b'8' => 127,
        b'9' => 57,
        b'?' => 127,
        b'@' => 0,
        b'\\' => 28,
        b']' => 29,
        b'^' => 30,
        b'_' => 31,
        b'a' => 1,
        b'b' => 2,
        b'c' => 3,
        b'd' => 4,
        b'e' => 5,
        b'f' => 6,
        b'g' => 7,
        b'h' => 8,
        b'j' => 10,
        b'k' => 11,
        b'l' => 12,
        b'n' => 14,
        b'o' => 15,
        b'p' => 16,
        b'q' => 17,
        b'r' => 18,
        b's' => 19,
        b't' => 20,
        b'u' => 21,
        b'v' => 22,
        b'w' => 23,
        b'x' => 24,
        b'y' => 25,
        b'z' => 26,
        b'~' => 30,
        // `i`, `m` and `[` are CSI u (fixterms).
        _ => return None,
    })
}

/// An ASCII control (libc `iscntrl`).
const fn is_control(cp: u32) -> bool {
    cp < 0x20 || cp == 0x7F
}

/// Exactly one byte, and it is a control.
fn is_control_utf8(s: &[u8]) -> bool {
    matches!(s, [b] if is_control(u32::from(*b)))
}

/// The fixterms `CSI u` modifier bits (ghostty `key_encode.zig:837`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct CsiUMods {
    shift: bool,
    alt: bool,
    ctrl: bool,
}

impl CsiUMods {
    const fn from_input(mods: Mods) -> CsiUMods {
        CsiUMods { shift: mods.shift(), alt: mods.alt(), ctrl: mods.ctrl() }
    }

    /// The bitmask plus one, as sent.
    const fn seq_int(self) -> u8 {
        1 + (self.shift as u8) + ((self.alt as u8) << 1) + ((self.ctrl as u8) << 2)
    }
}

/// The kitty modifier bits (ghostty `key_encode.zig:905`). Hyper and meta are never set from
/// input, as in ghostty.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct KittyMods {
    shift: bool,
    alt: bool,
    ctrl: bool,
    super_key: bool,
    hyper: bool,
    meta: bool,
    caps_lock: bool,
    num_lock: bool,
}

impl KittyMods {
    const fn from_input(mods: Mods) -> KittyMods {
        KittyMods {
            shift: mods.shift(),
            alt: mods.alt(),
            ctrl: mods.ctrl(),
            super_key: mods.super_key(),
            hyper: false,
            meta: false,
            caps_lock: mods.caps_lock(),
            num_lock: mods.num_lock(),
        }
    }

    const fn shift(self) -> bool {
        self.shift
    }

    /// Whether these modifiers mean the key's text is not text (alt only when it is alt, not
    /// macOS option).
    const fn prevents_text(self, alt_prevents_text: bool) -> bool {
        (self.alt && alt_prevents_text) || self.ctrl || self.super_key || self.hyper || self.meta
    }

    const fn int(self) -> u8 {
        (self.shift as u8)
            | (self.alt as u8) << 1
            | (self.ctrl as u8) << 2
            | (self.super_key as u8) << 3
            | (self.hyper as u8) << 4
            | (self.meta as u8) << 5
            | (self.caps_lock as u8) << 6
            | (self.num_lock as u8) << 7
    }

    /// The bitmask plus one, as sent.
    const fn seq_int(self) -> u32 {
        self.int() as u32 + 1
    }
}

/// The kitty event type field. ghostty sends `:1` for presses only in the `CSI 1 ; m X` form.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum KittyEvent {
    None,
    Press,
    Repeat,
    Release,
}

impl KittyEvent {
    const fn code(self) -> u32 {
        match self {
            KittyEvent::None => 0,
            KittyEvent::Press => 1,
            KittyEvent::Repeat => 2,
            KittyEvent::Release => 3,
        }
    }
}

/// `CSI key:shifted:base ; mods:event ; text u` (or `~`, or `CSI 1 ; mods:event X`), ghostty
/// `key_encode.zig:975`.
#[derive(Clone, Copy, Debug)]
struct KittySequence<'a> {
    key: u32,
    final_byte: u8,
    mods: KittyMods,
    event: KittyEvent,
    alternates: [Option<u32>; 2],
    text: &'a [u8],
}

impl KittySequence<'_> {
    fn encode(&self, w: &mut Out<'_>) -> Result<(), BufferFull> {
        if self.final_byte == b'u' || self.final_byte == b'~' {
            self.encode_full(w)
        } else {
            self.encode_special(w)
        }
    }

    fn encode_full(&self, w: &mut Out<'_>) -> Result<(), BufferFull> {
        w.bytes(b"\x1B[")?;
        w.dec(self.key)?;
        if let Some(shifted) = self.alternates[0] {
            w.byte(b':')?;
            w.dec(shifted)?;
        }
        if let Some(base) = self.alternates[1] {
            w.bytes(if self.alternates[0].is_none() { b"::" } else { b":" })?;
            w.dec(base)?;
        }

        let mods = self.mods.seq_int();
        let mut emit_prior = false;
        if self.event != KittyEvent::None && self.event != KittyEvent::Press {
            w.byte(b';')?;
            w.dec(mods)?;
            w.byte(b':')?;
            w.dec(self.event.code())?;
            emit_prior = true;
        } else if mods > 1 {
            w.byte(b';')?;
            w.dec(mods)?;
            emit_prior = true;
        }

        // Text, control characters skipped; invalid UTF-8 is no text.
        if let Ok(text) = core::str::from_utf8(self.text) {
            let mut count = 0usize;
            for c in text.chars() {
                let cp = u32::from(c);
                if is_control(cp) {
                    continue;
                }
                if count == 0 {
                    if !emit_prior {
                        w.byte(b';')?;
                    }
                    w.byte(b';')?;
                } else {
                    w.byte(b':')?;
                }
                w.dec(cp)?;
                count += 1;
            }
        }

        w.byte(self.final_byte)
    }

    fn encode_special(&self, w: &mut Out<'_>) -> Result<(), BufferFull> {
        let mods = self.mods.seq_int();
        if self.event != KittyEvent::None {
            w.bytes(b"\x1B[1;")?;
            w.dec(mods)?;
            w.byte(b':')?;
            w.dec(self.event.code())?;
            return w.byte(self.final_byte);
        }
        if mods > 1 {
            w.bytes(b"\x1B[1;")?;
            w.dec(mods)?;
            return w.byte(self.final_byte);
        }
        w.bytes(b"\x1B[")?;
        w.byte(self.final_byte)
    }
}
