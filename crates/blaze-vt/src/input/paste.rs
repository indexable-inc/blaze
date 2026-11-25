//! Paste encoding, a port of ghostty's `src/input/paste.zig`.
//!
//! Bytes that could drive the shell or the line discipline become spaces whatever the mode
//! (xterm's list). With bracketed paste (DEC 2004) the data goes between `CSI 200 ~` and
//! `CSI 201 ~`; without it every LF becomes CR, as typing Enter would. [`is_safe`] is the check a
//! host runs before pasting without asking: a newline or the bracketed-paste end sequence in the
//! data is suspect in any mode.
//!
//! The result is three slices (prefix, data, suffix) so nothing is copied or allocated: the
//! data is edited in place, or, from a borrowed slice, used as is when nothing needs editing.

/// What changes how a paste encodes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct PasteOpts {
    /// Bracketed paste mode (DEC 2004).
    pub bracketed: bool,
}

/// A paste ready for the PTY: write `prefix`, `data`, `suffix` in that order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pasted<'a> {
    pub prefix: &'static [u8],
    pub data: &'a [u8],
    pub suffix: &'static [u8],
}

/// A borrowed paste needs editing (an unsafe byte, or a newline outside bracketed paste): copy it
/// and call [`encode`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MutableRequired;

/// The bracketed-paste start, `CSI 200 ~`.
pub const BRACKET_START: &[u8] = b"\x1b[200~";
/// The bracketed-paste end, `CSI 201 ~`.
pub const BRACKET_END: &[u8] = b"\x1b[201~";

/// Bytes always replaced by a space, from xterm (ghostty `paste.zig:43`). The tty characters
/// (VINTR and on) can be changed by the program with `tcsetattr`; like ghostty, the defaults
/// are assumed.
pub const STRIP: [u8; 16] = [
    0x00, // NUL
    0x08, // BS
    0x05, // ENQ
    0x04, // EOT
    0x1B, // ESC
    0x7F, // DEL
    0x03, // VINTR (Ctrl+C)
    0x1C, // VQUIT (Ctrl+\)
    0x15, // VKILL (Ctrl+U)
    0x1A, // VSUSP (Ctrl+Z)
    0x11, // VSTART (Ctrl+Q)
    0x13, // VSTOP (Ctrl+S)
    0x17, // VWERASE (Ctrl+W)
    0x16, // VLNEXT (Ctrl+V)
    0x12, // VREPRINT (Ctrl+R)
    0x0F, // VDISCARD (Ctrl+O)
];

const fn stripped(b: u8) -> bool {
    let mut i = 0;
    while i < STRIP.len() {
        if STRIP[i] == b {
            return true;
        }
        i += 1;
    }
    false
}

const fn frame(bracketed: bool) -> (&'static [u8], &'static [u8]) {
    if bracketed { (BRACKET_START, BRACKET_END) } else { (b"", b"") }
}

/// Encodes `data` in place: unsafe bytes to spaces, and LF to CR unless bracketed. The data is
/// not checked for safety; see [`is_safe`].
pub fn encode(data: &mut [u8], opts: PasteOpts) -> Pasted<'_> {
    for b in data.iter_mut() {
        if stripped(*b) {
            *b = b' ';
        } else if *b == b'\n' && !opts.bracketed {
            // `\r\n` becomes `\r\r`, as in xterm.
            *b = b'\r';
        }
    }
    let (prefix, suffix) = frame(opts.bracketed);
    Pasted { prefix, data, suffix }
}

/// Encodes borrowed `data` when it needs no editing (ghostty's `[]const u8` form).
pub fn encode_borrowed(data: &[u8], opts: PasteOpts) -> Result<Pasted<'_>, MutableRequired> {
    if data.iter().any(|&b| stripped(b) || (b == b'\n' && !opts.bracketed)) {
        return Err(MutableRequired);
    }
    let (prefix, suffix) = frame(opts.bracketed);
    Ok(Pasted { prefix, data, suffix })
}

/// Whether `data` looks safe to paste without asking: no `\n` (it can run a command) and no
/// `CSI 201 ~` (it can end a bracketed paste early), in any mode (ghostty `paste.zig:140`).
pub fn is_safe(data: &[u8]) -> bool {
    !data.contains(&b'\n') && !data.windows(BRACKET_END.len()).any(|w| w == BRACKET_END)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    const BRACKETED: PasteOpts = PasteOpts { bracketed: true };
    const PLAIN: PasteOpts = PasteOpts { bracketed: false };

    // ghostty paste.zig:136 "isSafe"
    #[test]
    fn is_safe_cases() {
        assert!(is_safe(b"hello"));
        assert!(!is_safe(b"hello\n"));
        assert!(!is_safe(b"hello\nworld"));
        assert!(!is_safe(b"he\x1b[201~llo"));
    }

    // ghostty paste.zig:144 "encode bracketed"
    #[test]
    fn encode_bracketed() {
        let r = encode_borrowed(b"hello", BRACKETED).unwrap();
        assert_eq!(r.prefix, b"\x1b[200~");
        assert_eq!(r.data, b"hello");
        assert_eq!(r.suffix, b"\x1b[201~");
    }

    // ghostty paste.zig:155 "encode unbracketed no newlines"
    #[test]
    fn encode_unbracketed_no_newlines() {
        let r = encode_borrowed(b"hello", PLAIN).unwrap();
        assert_eq!((r.prefix, r.data, r.suffix), (&b""[..], &b"hello"[..], &b""[..]));
    }

    // ghostty paste.zig:166 "encode unbracketed newlines const"
    #[test]
    fn encode_unbracketed_newlines_const() {
        assert_eq!(encode_borrowed(b"hello\nworld", PLAIN), Err(MutableRequired));
    }

    // ghostty paste.zig:174 "encode unbracketed newlines"
    #[test]
    fn encode_unbracketed_newlines() {
        let mut data = b"hello\nworld".to_vec();
        let r = encode(&mut data, PLAIN);
        assert_eq!((r.prefix, r.data, r.suffix), (&b""[..], &b"hello\rworld"[..], &b""[..]));
    }

    // ghostty paste.zig:184 "encode unbracketed windows-stye newline"
    #[test]
    fn encode_unbracketed_windows_style_newline() {
        let mut data = b"hello\r\nworld".to_vec();
        assert_eq!(encode(&mut data, PLAIN).data, b"hello\r\rworld");
    }

    // ghostty paste.zig:194 "encode strip unsafe bytes const"
    #[test]
    fn encode_strip_unsafe_bytes_const() {
        assert_eq!(encode_borrowed(b"hello\x00world", BRACKETED), Err(MutableRequired));
    }

    // ghostty paste.zig:202 "encode strip unsafe bytes mutable bracketed"
    #[test]
    fn encode_strip_unsafe_bytes_mutable_bracketed() {
        let mut data = b"hel\x1blo\x00world".to_vec();
        let r = encode(&mut data, BRACKETED);
        assert_eq!(
            (r.prefix, r.data, r.suffix),
            (&b"\x1b[200~"[..], &b"hel lo world"[..], &b"\x1b[201~"[..])
        );
    }

    // ghostty paste.zig:212 "encode strip unsafe bytes mutable unbracketed"
    #[test]
    fn encode_strip_unsafe_bytes_mutable_unbracketed() {
        let mut data = b"hel\x03lo".to_vec();
        let r = encode(&mut data, PLAIN);
        assert_eq!((r.prefix, r.data, r.suffix), (&b""[..], &b"hel lo"[..], &b""[..]));
    }

    // ghostty paste.zig:222 "encode strip multiple unsafe bytes"
    #[test]
    fn encode_strip_multiple_unsafe_bytes() {
        let mut data = b"\x00\x08\x7f".to_vec();
        assert_eq!(encode(&mut data, BRACKETED).data, b"   ");
    }

    // Beyond ghostty's tests.

    #[test]
    fn every_strip_byte_becomes_a_space_and_nothing_else_changes() {
        for b in 0..=255u8 {
            for opts in [BRACKETED, PLAIN] {
                let mut data = [b];
                let out = encode(&mut data, opts).data[0];
                let want = if STRIP.contains(&b) {
                    b' '
                } else if b == b'\n' && !opts.bracketed {
                    b'\r'
                } else {
                    b
                };
                assert_eq!(out, want, "byte {b:#04x} {opts:?}");
            }
        }
    }

    #[test]
    fn a_bracketed_paste_cannot_end_itself_early() {
        // ESC is stripped, so the end sequence inside the data can never reach the PTY whole.
        let mut data = b"a\x1b[201~rm -rf ~\n".to_vec();
        assert!(!is_safe(&data));
        let r = encode(&mut data, BRACKETED);
        assert!(!r.data.windows(BRACKET_END.len()).any(|w| w == BRACKET_END));
        assert_eq!(r.data, b"a [201~rm -rf ~\n");
    }

    #[test]
    fn borrowed_agrees_with_in_place() {
        blaze_sim::check(blaze_sim::Config::cases(1000), |tape| {
            let data: Vec<u8> = tape.vec(0..=24, "data", |t| t.int_in(0..=0x7Fu8, "b"));
            let opts = PasteOpts { bracketed: tape.coin("bracketed") };
            let mut copy = data.clone();
            let in_place = encode(&mut copy, opts);
            match encode_borrowed(&data, opts) {
                Ok(b) => assert_eq!(b, in_place),
                Err(MutableRequired) => assert_ne!(in_place.data, &data[..]),
            }
        });
    }
}
