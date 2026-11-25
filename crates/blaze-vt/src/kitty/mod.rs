//! APC: kitty graphics and the glyph protocol (vt-inventory.md section 9; ghostty's
//! `src/terminal/kitty/graphics_*.zig`, `apc.zig`, `apc/glyph/*`).

pub mod apc;
pub mod command;
pub mod exec;
pub mod glyph;
pub mod image;
mod num;
pub mod storage;
pub mod unicode;

/// A reply's bytes, built in place: the longest kitty or glyph reply fits.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Wire {
    buf: [u8; Wire::CAP],
    len: usize,
}

impl Wire {
    /// The most bytes a reply may have.
    pub const CAP: usize = 256;

    /// An empty reply.
    pub const fn new() -> Wire {
        Wire { buf: [0; Wire::CAP], len: 0 }
    }

    /// Append bytes. Every reply is built from bounded parts, so overflow is a bug.
    pub fn put(&mut self, bytes: &[u8]) {
        let end = self.len + bytes.len();
        assert!(end <= Wire::CAP, "reply longer than Wire::CAP");
        self.buf[self.len..end].copy_from_slice(bytes);
        self.len = end;
    }

    /// Append `n` in decimal.
    pub fn put_decimal(&mut self, mut n: u64) {
        let mut digits = [0u8; 20];
        let mut at = digits.len();
        loop {
            at -= 1;
            digits[at] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        self.put(&digits[at..]);
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Default for Wire {
    fn default() -> Wire {
        Wire::new()
    }
}

impl core::fmt::Debug for Wire {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Wire(\"{}\")", self.as_bytes().escape_ascii())
    }
}
