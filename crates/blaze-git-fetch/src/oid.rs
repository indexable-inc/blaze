//! Git's object id: the SHA-1 of an object's kind, length and bytes. A name, never a proof
//! (see [`crate::sha1`]).

#![blaze::sans_io]

use core::fmt;

use crate::sha1::Sha1;

/// An object id.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Oid(pub [u8; 20]);

/// An object's kind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl Kind {
    pub const fn name(self) -> &'static str {
        match self {
            Kind::Commit => "commit",
            Kind::Tree => "tree",
            Kind::Blob => "blob",
            Kind::Tag => "tag",
        }
    }

    /// The kind of a pack entry's type number, if it is one of the four.
    pub(crate) const fn of_pack(number: u8) -> Option<Kind> {
        match number {
            1 => Some(Kind::Commit),
            2 => Some(Kind::Tree),
            3 => Some(Kind::Blob),
            4 => Some(Kind::Tag),
            _ => None,
        }
    }
}

impl Oid {
    /// The id of an object of `kind` holding `bytes`.
    pub fn of(kind: Kind, bytes: &[u8]) -> Oid {
        let mut h = Sha1::new();
        h.update(kind.name().as_bytes());
        h.update(b" ");
        let mut digits = [0u8; 20];
        let mut at = digits.len();
        let mut n = bytes.len();
        loop {
            at -= 1;
            digits[at] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        h.update(&digits[at..]);
        h.update(b"\0");
        h.update(bytes);
        Oid(h.finish())
    }

    /// Forty lower- or upper-case hex digits.
    pub fn parse_hex(hex: &[u8]) -> Option<Oid> {
        if hex.len() != 40 {
            return None;
        }
        let mut out = [0u8; 20];
        for (byte, pair) in out.iter_mut().zip(hex.chunks_exact(2)) {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            *byte = (hi * 16 + lo) as u8;
        }
        Some(Oid(out))
    }

    /// Its forty lower-case hex digits.
    pub fn hex(&self) -> [u8; 40] {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut out = [0u8; 40];
        for (pair, byte) in out.chunks_exact_mut(2).zip(self.0) {
            pair[0] = DIGITS[usize::from(byte >> 4)];
            pair[1] = DIGITS[usize::from(byte & 15)];
        }
        out
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hex = self.hex();
        // Hex digits are ASCII.
        f.write_str(core::str::from_utf8(&hex).map_err(|_| fmt::Error)?)
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Oid({self})")
    }
}
