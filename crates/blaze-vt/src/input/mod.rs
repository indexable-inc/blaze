//! Input encoding: what the host sends to the PTY for a key, a mouse event, a paste or a focus
//! change, as ghostty encodes it (vt-inventory.md §10; ghostty `src/input/key_encode.zig`,
//! `function_keys.zig`, `kitty.zig`, `mouse_encode.zig`, `paste.zig`, `terminal/focus.zig`).
//!
//! No terminal state lives here. Each encoder takes the event and an options struct of plain
//! data (the modes the terminal core holds: DECCKM, DECKPAM, DECBKM, 1035, 1036,
//! modifyOtherKeys, the kitty keyboard flags, the mouse tracking mode and format, bracketed
//! paste, focus reporting) that the caller fills, and writes bytes into a buffer the caller
//! owns. Nothing allocates.
//!
//! | module | what | ghostty |
//! |---|---|---|
//! | [`key`] | [`key::KeyEvent`] and [`key::EncodeOpts`] to bytes: legacy (PC-style function keys, C0, alt-ESC, fixterms CSI u), modifyOtherKeys 2, the kitty keyboard protocol flags 1–16 | key_encode.zig |
//! | `function_keys` | the legacy PC-style function key table | function_keys.zig |
//! | `kitty_keys` | the kitty functional key table | kitty.zig |
//! | [`mouse`] | [`mouse::MouseEvent`] to bytes: tracking modes 9/1000/1002/1003, formats X10, 1005, 1006, 1015, 1016 | mouse_encode.zig |
//! | [`paste`] | unsafe-byte replacement, LF to CR, bracketed paste (2004), the unsafe-paste check | paste.zig |
//! | [`focus`] | `CSI I` / `CSI O` (1004) | terminal/focus.zig |

pub mod focus;
mod function_keys;
pub mod key;
mod kitty_keys;
pub mod mouse;
pub mod paste;

#[cfg(test)]
mod inventory_tests;

/// The caller's buffer was too small for the encoding. Nothing usable was written; a buffer of
/// the size the encoder's `max_len` names always fits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BufferFull {
    /// The buffer's length.
    pub capacity: usize,
}

/// A writer over the caller's buffer: each push either fits or fails the whole encoding.
pub(crate) struct Out<'o> {
    buf: &'o mut [u8],
    len: usize,
}

impl<'o> Out<'o> {
    pub(crate) fn new(buf: &'o mut [u8]) -> Out<'o> {
        Out { buf, len: 0 }
    }

    fn full(&self) -> BufferFull {
        BufferFull { capacity: self.buf.len() }
    }

    pub(crate) fn byte(&mut self, b: u8) -> Result<(), BufferFull> {
        let full = self.full();
        let slot = self.buf.get_mut(self.len).ok_or(full)?;
        *slot = b;
        self.len += 1;
        Ok(())
    }

    pub(crate) fn bytes(&mut self, bytes: &[u8]) -> Result<(), BufferFull> {
        let full = self.full();
        let end = self.len.checked_add(bytes.len()).ok_or(full)?;
        let dst = self.buf.get_mut(self.len..end).ok_or(full)?;
        dst.copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }

    /// `n` in decimal, as zig's `{d}` prints it.
    pub(crate) fn dec(&mut self, n: u32) -> Result<(), BufferFull> {
        let mut digits = [0u8; 10];
        let mut at = digits.len();
        let mut n = n;
        loop {
            at -= 1;
            digits[at] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        self.bytes(&digits[at..])
    }

    /// A signed decimal (SGR-pixels coordinates may be negative).
    pub(crate) fn dec_signed(&mut self, n: i32) -> Result<(), BufferFull> {
        if n < 0 {
            self.byte(b'-')?;
        }
        self.dec(n.unsigned_abs())
    }

    pub(crate) fn char(&mut self, c: char) -> Result<(), BufferFull> {
        let mut utf8 = [0u8; 4];
        self.bytes(c.encode_utf8(&mut utf8).as_bytes())
    }

    pub(crate) fn finish(self) -> &'o [u8] {
        &self.buf[..self.len]
    }
}
