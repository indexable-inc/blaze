//! [`Put`]: where text goes. Three sinks: blaze-own's [`String<A>`] (grows in the allocator it
//! names), [`Buf<N>`] (a fixed stack buffer that refuses when full) and [`Count`] (measures
//! only, so a buffer is sized exactly before it is filled).

use core::alloc::Allocator;

use blaze_own::String;

/// A sink of UTF-8 text. A writer never fails at the call: a sink that can run out
/// ([`Buf`]) remembers that it did and says so when it is read.
pub trait Put {
    /// `text` at the end.
    fn put_str(&mut self, text: &str);

    /// One ASCII byte at the end.
    ///
    /// # Panics
    ///
    /// When `byte` is not ASCII (a bug at the caller: half a character is not text). For a
    /// literal byte the check folds away.
    #[inline]
    fn put_byte(&mut self, byte: u8) {
        assert!(byte.is_ascii(), "blaze_text::Put::put_byte: 0x{byte:02x} is not ASCII");
        let one = [byte];
        match core::str::from_utf8(&one) {
            Ok(text) => self.put_str(text),
            Err(_) => unreachable!("an ASCII byte is UTF-8"),
        }
    }

    /// Room for `additional` more bytes, where the sink has a notion of room; a hint.
    #[inline]
    fn reserve(&mut self, additional: usize) {
        let _ = additional;
    }
}

impl<P: Put + ?Sized> Put for &mut P {
    #[inline]
    fn put_str(&mut self, text: &str) {
        (**self).put_str(text);
    }

    #[inline]
    fn put_byte(&mut self, byte: u8) {
        (**self).put_byte(byte);
    }

    #[inline]
    fn reserve(&mut self, additional: usize) {
        (**self).reserve(additional);
    }
}

/// Grows in its allocator; an allocator that refuses panics with blaze-own's `AllocFailure`,
/// as `push_str` does (a `Count` first, then `try_reserve`, is the form that cannot).
impl<A: Allocator> Put for String<A> {
    #[inline]
    fn put_str(&mut self, text: &str) {
        self.push_str(text);
    }

    #[inline]
    fn reserve(&mut self, additional: usize) {
        String::reserve(self, additional);
    }
}

/// What a full [`Buf`] says: its room and the bytes the writes wanted in all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Overflow {
    pub capacity: usize,
    pub needed: usize,
}

/// Up to `N` bytes of text on the stack. A write that does not fit is refused whole and the
/// buffer is marked full: every later write is refused too, so the text is never a silently
/// cut one. [`Buf::text`] gives the text, or the [`Overflow`] with the bytes it needed.
#[derive(Clone, Copy)]
pub struct Buf<const N: usize> {
    /// Bytes `..len` are UTF-8, written only from whole `str`s.
    bytes: [u8; N],
    len: usize,
    /// The bytes every write asked for, the refused ones too.
    needed: usize,
    full: bool,
}

impl<const N: usize> Buf<N> {
    pub const fn new() -> Buf<N> {
        Buf { bytes: [0; N], len: 0, needed: 0, full: false }
    }

    /// The text, or the [`Overflow`] when any write was refused.
    pub fn text(&self) -> Result<&str, Overflow> {
        if self.full {
            return Err(Overflow { capacity: N, needed: self.needed });
        }
        match core::str::from_utf8(&self.bytes[..self.len]) {
            Ok(text) => Ok(text),
            Err(_) => unreachable!("a `Buf` holds whole `str`s only"),
        }
    }

    /// Empties it, the full mark too.
    pub fn clear(&mut self) {
        self.len = 0;
        self.needed = 0;
        self.full = false;
    }

    pub const fn capacity(&self) -> usize {
        N
    }
}

impl<const N: usize> Default for Buf<N> {
    fn default() -> Buf<N> {
        Buf::new()
    }
}

impl<const N: usize> Put for Buf<N> {
    #[inline]
    fn put_str(&mut self, text: &str) {
        self.needed = self.needed.saturating_add(text.len());
        let end = self.len + text.len();
        if self.full || end > N {
            self.full = true;
            return;
        }
        self.bytes[self.len..end].copy_from_slice(text.as_bytes());
        self.len = end;
    }
}

/// Counts what would be written: bytes (to size a buffer) and characters (to pad a column).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Count {
    pub bytes: usize,
    pub chars: usize,
}

impl Count {
    pub const fn new() -> Count {
        Count { bytes: 0, chars: 0 }
    }
}

impl Put for Count {
    #[inline]
    fn put_str(&mut self, text: &str) {
        self.bytes += text.len();
        // A character starts at every byte that is not a continuation byte.
        self.chars += text.bytes().filter(|&b| b & 0xC0 != 0x80).count();
    }

    #[inline]
    fn put_byte(&mut self, _byte: u8) {
        self.bytes += 1;
        self.chars += 1;
    }
}
