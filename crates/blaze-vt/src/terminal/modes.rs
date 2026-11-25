//! The modes a program sets with SM/RM and DECSET/DECRST, as ghostty's modes.zig lists them:
//! each a number, ANSI or DEC, with a default. One saved slot per mode (XTSAVE/XTRESTORE).

/// A mode ghostty knows (modes.zig:251).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// ANSI 2, KAM.
    DisableKeyboard,
    /// ANSI 4, IRM.
    Insert,
    /// ANSI 12, SRM (on).
    SendReceive,
    /// ANSI 20, LNM.
    Linefeed,
    /// ?1, DECCKM.
    CursorKeys,
    /// ?3, DECCOLM.
    Col132,
    /// ?4, DECSCLM.
    SlowScroll,
    /// ?5, DECSCNM.
    ReverseColors,
    /// ?6, DECOM.
    Origin,
    /// ?7, DECAWM (on).
    Wraparound,
    /// ?8, DECARM.
    Autorepeat,
    /// ?9, X10 mouse.
    MouseX10,
    /// ?12.
    CursorBlinking,
    /// ?25, DECTCEM (on).
    CursorVisible,
    /// ?40, allow DECCOLM.
    EnableMode3,
    /// ?45.
    ReverseWrap,
    /// ?47.
    AltScreenLegacy,
    /// ?66, DECNKM.
    KeypadKeys,
    /// ?67, DECBKM.
    BackarrowKey,
    /// ?69, DECLRMM.
    EnableLeftRightMargin,
    /// ?1000.
    MouseNormal,
    /// ?1002.
    MouseButton,
    /// ?1003.
    MouseAny,
    /// ?1004.
    FocusEvent,
    /// ?1005.
    MouseUtf8,
    /// ?1006.
    MouseSgr,
    /// ?1007 (on).
    MouseAlternateScroll,
    /// ?1015.
    MouseUrxvt,
    /// ?1016.
    MouseSgrPixels,
    /// ?1035 (on).
    IgnoreKeypadWithNumlock,
    /// ?1036 (on).
    AltEscPrefix,
    /// ?1039.
    AltSendsEscape,
    /// ?1045.
    ReverseWrapExtended,
    /// ?1047.
    AltScreen,
    /// ?1048.
    SaveCursor,
    /// ?1049.
    AltScreenSaveCursorClear,
    /// ?2004.
    BracketedPaste,
    /// ?2026.
    SynchronizedOutput,
    /// ?2027.
    GraphemeCluster,
    /// ?2031.
    ReportColorScheme,
    /// ?2048.
    InBandSizeReports,
}

/// Every mode: itself, its number, whether it is ANSI, its default.
pub const MODES: [(Mode, u16, bool, bool); 41] = [
    (Mode::DisableKeyboard, 2, true, false),
    (Mode::Insert, 4, true, false),
    (Mode::SendReceive, 12, true, true),
    (Mode::Linefeed, 20, true, false),
    (Mode::CursorKeys, 1, false, false),
    (Mode::Col132, 3, false, false),
    (Mode::SlowScroll, 4, false, false),
    (Mode::ReverseColors, 5, false, false),
    (Mode::Origin, 6, false, false),
    (Mode::Wraparound, 7, false, true),
    (Mode::Autorepeat, 8, false, false),
    (Mode::MouseX10, 9, false, false),
    (Mode::CursorBlinking, 12, false, false),
    (Mode::CursorVisible, 25, false, true),
    (Mode::EnableMode3, 40, false, false),
    (Mode::ReverseWrap, 45, false, false),
    (Mode::AltScreenLegacy, 47, false, false),
    (Mode::KeypadKeys, 66, false, false),
    (Mode::BackarrowKey, 67, false, false),
    (Mode::EnableLeftRightMargin, 69, false, false),
    (Mode::MouseNormal, 1000, false, false),
    (Mode::MouseButton, 1002, false, false),
    (Mode::MouseAny, 1003, false, false),
    (Mode::FocusEvent, 1004, false, false),
    (Mode::MouseUtf8, 1005, false, false),
    (Mode::MouseSgr, 1006, false, false),
    (Mode::MouseAlternateScroll, 1007, false, true),
    (Mode::MouseUrxvt, 1015, false, false),
    (Mode::MouseSgrPixels, 1016, false, false),
    (Mode::IgnoreKeypadWithNumlock, 1035, false, true),
    (Mode::AltEscPrefix, 1036, false, true),
    (Mode::AltSendsEscape, 1039, false, false),
    (Mode::ReverseWrapExtended, 1045, false, false),
    (Mode::AltScreen, 1047, false, false),
    (Mode::SaveCursor, 1048, false, false),
    (Mode::AltScreenSaveCursorClear, 1049, false, false),
    (Mode::BracketedPaste, 2004, false, false),
    (Mode::SynchronizedOutput, 2026, false, false),
    (Mode::GraphemeCluster, 2027, false, false),
    (Mode::ReportColorScheme, 2031, false, false),
    (Mode::InBandSizeReports, 2048, false, false),
];

impl Mode {
    /// The mode numbered `value` (ANSI or DEC), if ghostty knows it (modes.zig `modeFromInt`).
    pub fn from_int(value: u16, ansi: bool) -> Option<Mode> {
        MODES.iter().find(|m| m.1 == value && m.2 == ansi).map(|m| m.0)
    }

    pub const fn bit(self) -> u64 {
        1 << self as u8
    }

    pub fn value(self) -> u16 {
        MODES[self as usize].1
    }

    pub fn is_ansi(self) -> bool {
        MODES[self as usize].2
    }
}

/// The modes' values, saved values and defaults, a bit each.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ModeState {
    values: u64,
    saved: u64,
    defaults: u64,
}

impl ModeState {
    /// Every mode at its default, with `grapheme_cluster` (2027) as the caller chose.
    pub fn new(grapheme_cluster: bool) -> ModeState {
        let mut defaults = 0;
        for (m, _, _, d) in MODES {
            if d {
                defaults |= m.bit();
            }
        }
        if grapheme_cluster {
            defaults |= Mode::GraphemeCluster.bit();
        }
        ModeState { values: defaults, saved: defaults, defaults }
    }

    pub fn get(&self, m: Mode) -> bool {
        self.values & m.bit() != 0
    }

    pub fn set(&mut self, m: Mode, on: bool) {
        if on {
            self.values |= m.bit();
        } else {
            self.values &= !m.bit();
        }
    }

    /// XTSAVE.
    pub fn save(&mut self, m: Mode) {
        self.saved = (self.saved & !m.bit()) | (self.values & m.bit());
    }

    /// XTRESTORE: the saved value, set (modes.zig `restore`).
    pub fn restore(&mut self, m: Mode) -> bool {
        let v = self.saved & m.bit() != 0;
        self.set(m, v);
        v
    }

    /// Every mode to its default, the saved values too (modes.zig `reset`).
    pub fn reset(&mut self) {
        self.values = self.defaults;
        self.saved = self.defaults;
    }
}

/// The mouse tracking mode and report format live with their encoder.
pub use crate::input::mouse::{MouseEvents, MouseFormat};
