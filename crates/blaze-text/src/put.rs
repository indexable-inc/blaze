//! [`Put`]: where text goes. Three sinks: blaze-own's [`String<A>`] (grows in the allocator it
//! names), blaze-own's [`FixedString<N>`] (a fixed stack buffer that refuses when full) and
//! [`Count`] (measures only, so a buffer is sized exactly before it is filled).

use core::alloc::Allocator;

use blaze_own::{FixedString, String};

/// A sink of UTF-8 text. A writer never fails at the call: a sink that can run out
/// ([`FixedString`]) remembers that it did and says so when it is read.
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
    fn put_byte(&mut self, byte: u8) {
        assert!(byte.is_ascii(), "blaze_text::Put::put_byte: 0x{byte:02x} is not ASCII");
        self.push(char::from(byte));
    }

    #[inline]
    fn reserve(&mut self, additional: usize) {
        String::reserve(self, additional);
    }
}

/// Up to `N` bytes on the stack. A write that does not fit is refused whole, and every later
/// write too (the refusal is sticky until `clear`), so the text is never a silently cut one;
/// `FixedString::text` gives it, or the `Overflow` with the bytes all writes wanted.
impl<const N: usize> Put for FixedString<N> {
    #[inline]
    fn put_str(&mut self, text: &str) {
        // The refusal and the count are kept inside; `text()` reports them.
        let _ = self.try_push_str(text);
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

/// A string in `alloc` from one chain of writers, so a format site is one expression:
/// `text_in(Global, |out| out.str("x = ").int(x))`. It grows as it is written; for one
/// allocation at the exact size, [`text_exact_in`].
#[inline]
pub fn text_in<A: Allocator>(alloc: A, write: impl for<'o> FnOnce(&'o mut String<A>) -> &'o mut String<A>) -> String<A> {
    let mut out = String::new_in(alloc);
    write(&mut out);
    out
}

/// A string in `alloc` from one chain of writers, allocated once at its exact size: the chain
/// is run twice, first into a [`Count`], then into a string of that capacity. For a string
/// that is kept (a key, a record's text), where `text_in`'s growth would leave slack.
#[inline]
pub fn text_exact_in<A: Allocator>(alloc: A, write: impl for<'o> Fn(&'o mut dyn Put) -> &'o mut dyn Put) -> String<A> {
    let mut count = Count::new();
    write(&mut count);
    let mut out = String::with_capacity_in(count.bytes, alloc);
    write(&mut out);
    out
}
