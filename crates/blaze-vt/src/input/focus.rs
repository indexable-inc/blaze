//! Focus reports, ghostty's `src/terminal/focus.zig`: `CSI I` when the surface gains focus and
//! `CSI O` when it loses it, sent only while focus reporting (DEC 1004) is set (ghostty's
//! termio checks the mode; here [`FocusOpts`] carries it).

/// A focus change.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FocusEvent {
    Gained,
    Lost,
}

/// What changes whether focus is reported.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct FocusOpts {
    /// DEC mode 1004.
    pub reporting: bool,
}

/// The most bytes a focus report has (ghostty `max_encode_size`).
pub const MAX_LEN: usize = 3;

/// The report for `event`: empty unless 1004 is set.
pub const fn encode(event: FocusEvent, opts: FocusOpts) -> &'static [u8] {
    if !opts.reporting {
        return b"";
    }
    match event {
        FocusEvent::Gained => b"\x1B[I",
        FocusEvent::Lost => b"\x1B[O",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ON: FocusOpts = FocusOpts { reporting: true };

    // ghostty terminal/focus.zig:27 "encode focus gained"
    #[test]
    fn encode_focus_gained() {
        assert_eq!(encode(FocusEvent::Gained, ON), b"\x1B[I");
        assert!(encode(FocusEvent::Gained, ON).len() <= MAX_LEN);
    }

    // ghostty terminal/focus.zig:34 "encode focus lost"
    #[test]
    fn encode_focus_lost() {
        assert_eq!(encode(FocusEvent::Lost, ON), b"\x1B[O");
    }

    #[test]
    fn nothing_without_mode_1004() {
        assert_eq!(encode(FocusEvent::Gained, FocusOpts::default()), b"");
        assert_eq!(encode(FocusEvent::Lost, FocusOpts::default()), b"");
    }
}
