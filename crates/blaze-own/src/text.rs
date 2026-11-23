//! [`String`]: growable UTF-8 text in an allocator the caller names, over this crate's
//! [`Vec<u8, A>`](crate::Vec); [`format_in!`] is `format!` into an allocator and
//! [`format_into!`] `format!` appended to the caller's buffer.
//!
//! Moved here from `blaze_mem::text`, which re-exports it (so `blaze_mem::String` is this
//! type); blaze-mem keeps `OsString` and `PathBuf`, which need std's `OsStr`.
//!
//! Like every owner here it has no default allocator. It compares, orders and hashes as `str`
//! does, and borrows as it, so it is a map key looked up by `&str`. With the `std` feature it is
//! also `AsRef<OsStr>` and `AsRef<Path>` (impls only this crate may write, by the orphan rule),
//! so it is passed where std takes a path or an argument.

use core::{
    alloc::Allocator,
    borrow::{Borrow, BorrowMut},
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    ops::{Deref, DerefMut},
    str::{self, Utf8Error},
};

use crate::{AllocFailure, Box, Vec};

/// A growable UTF-8 string in `A`: std's `String` with an allocator.
pub struct String<A: Allocator> {
    /// UTF-8 whenever a method returns: only `str`s, `char`s and bytes checked by
    /// [`String::from_utf8`] go in, and only whole characters come out.
    bytes: Vec<u8, A>,
}

impl<A: Allocator> String<A> {
    /// An empty string in `alloc`; nothing is allocated until it grows.
    #[inline]
    pub fn new_in(alloc: A) -> String<A> {
        String { bytes: Vec::new_in(alloc) }
    }

    /// An empty string in `alloc` with room for `capacity` bytes.
    #[inline]
    pub fn with_capacity_in(capacity: usize, alloc: A) -> String<A> {
        String { bytes: Vec::with_capacity_in(capacity, alloc) }
    }

    /// A copy of `text` in `alloc`.
    #[inline]
    pub fn from_str_in(text: &str, alloc: A) -> String<A> {
        let mut out = String::with_capacity_in(text.len(), alloc);
        out.push_str(text);
        out
    }

    /// `bytes` as a string, when they are UTF-8; else the bytes back, with where they stop
    /// being UTF-8.
    pub fn from_utf8(bytes: Vec<u8, A>) -> Result<String<A>, FromUtf8Error<A>> {
        match str::from_utf8(bytes.as_slice()) {
            Ok(_) => Ok(String { bytes }),
            Err(error) => Err(FromUtf8Error { bytes, error }),
        }
    }

    /// An empty string in `alloc` with room for `capacity` bytes, or why there is none.
    #[inline]
    pub fn try_with_capacity_in(capacity: usize, alloc: A) -> Result<String<A>, AllocFailure> {
        Ok(String { bytes: Vec::try_with_capacity_in(capacity, alloc)? })
    }

    /// A copy of `text` in `alloc`, or why there is no room for it.
    #[inline]
    pub fn try_from_str_in(text: &str, alloc: A) -> Result<String<A>, AllocFailure> {
        let mut out = String::try_with_capacity_in(text.len(), alloc)?;
        out.try_push_str(text)?;
        Ok(out)
    }

    #[inline]
    pub fn push(&mut self, ch: char) {
        let mut buffer = [0; 4];
        self.bytes.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
    }

    /// [`String::push`], or why there is no room; the string is as it was.
    #[inline]
    pub fn try_push(&mut self, ch: char) -> Result<(), AllocFailure> {
        let mut buffer = [0; 4];
        self.try_push_str(ch.encode_utf8(&mut buffer))
    }

    #[inline]
    pub fn push_str(&mut self, text: &str) {
        self.bytes.extend_from_slice(text.as_bytes());
    }

    /// [`String::push_str`], or why there is no room; the string is as it was.
    #[inline]
    pub fn try_push_str(&mut self, text: &str) -> Result<(), AllocFailure> {
        self.bytes.try_extend_from_slice(text.as_bytes())
    }

    /// [`String::reserve`], or why there is no room; the string is as it was.
    #[inline]
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), AllocFailure> {
        self.bytes.try_reserve(additional)
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        // SAFETY: `bytes` is UTF-8 (the field's invariant).
        unsafe { str::from_utf8_unchecked(self.bytes.as_slice()) }
    }

    #[inline]
    pub fn as_mut_str(&mut self) -> &mut str {
        // SAFETY: `bytes` is UTF-8 (the field's invariant), and a `&mut str` can only be changed
        // in ways that keep it so.
        unsafe { str::from_utf8_unchecked_mut(self.bytes.as_mut_slice()) }
    }

    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    /// Its length in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The bytes it holds without growing.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }

    /// Room for at least `additional` more bytes.
    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        self.bytes.reserve(additional);
    }

    /// Empties it, keeping its capacity.
    #[inline]
    pub fn clear(&mut self) {
        self.bytes.clear();
    }

    /// Shortens it to `new_len` bytes; nothing when it is no longer.
    ///
    /// # Panics
    ///
    /// When `new_len` is inside a character.
    #[track_caller]
    pub fn truncate(&mut self, new_len: usize) {
        if new_len < self.len() {
            assert!(
                self.as_str().is_char_boundary(new_len),
                "blaze_own::String::truncate({new_len}): byte {new_len} of the {}-byte string is inside a character",
                self.len(),
            );
            self.bytes.truncate(new_len);
        }
    }

    /// Takes its last character off.
    pub fn pop(&mut self) -> Option<char> {
        let ch = self.as_str().chars().next_back()?;
        self.bytes.truncate(self.len() - ch.len_utf8());
        Some(ch)
    }

    /// The allocator its bytes are in.
    #[inline]
    pub fn allocator(&self) -> &A {
        self.bytes.allocator()
    }

    /// Its bytes, which are UTF-8.
    #[inline]
    pub fn into_bytes(self) -> Vec<u8, A> {
        self.bytes
    }

    /// The text as a `Box<str, A>`, its room shrunk to its length.
    /// Freezes this bump-allocated string into a `&'a mut str` tied to the arena's lifetime `'a`,
    /// with no UTF-8 re-validation and no heap leak.
    #[inline]
    pub fn into_bump_str<'a>(self) -> &'a mut str
    where
        A: crate::BumpAlloc + 'a,
    {
        let bytes = self.bytes.into_bump_slice();
        // SAFETY: `String` maintains valid UTF-8 for `self.bytes[..len]`.
        unsafe { str::from_utf8_unchecked_mut(bytes) }
    }

    pub fn into_boxed_str(self) -> Box<str, A> {
        let (raw, alloc) = Box::into_raw_with_allocator(self.bytes.into_boxed_slice());
        // SAFETY: `raw` and `alloc` are a `Box<[u8], A>` taken apart just above, whose bytes are
        // UTF-8 (the field's invariant); `str` has `[u8]`'s layout and metadata, so the cast
        // keeps the length and the box frees the same layout from the same allocator.
        unsafe { Box::from_raw_in(raw as *mut str, alloc) }
    }
}

/// What [`format_in!`] expands to: `args` written into a new string in `alloc`.
///
/// # Panics
///
/// When a `Display` or `Debug` implementation `args` call returns an error, as `format!`
/// does: writing to a string never fails, so that implementation is wrong.
#[doc(hidden)]
#[track_caller]
pub fn format<A: Allocator>(alloc: A, args: fmt::Arguments<'_>) -> String<A> {
    if let Some(text) = args.as_str() {
        return String::from_str_in(text, alloc);
    }
    // Room reserved once, by `format!`'s own estimate (twice the literal bytes; none when the
    // text starts with a placeholder and its literals are under 16 bytes), so the common text
    // grows its buffer at most rarely instead of once per doubling.
    let mut out = String::with_capacity_in(args.estimated_capacity(), alloc);
    if fmt::Write::write_fmt(&mut out, args).is_err() {
        panic!(
            "format_in!:a formatting trait implementation returned an error when writing to a string, which never fails"
        );
    }
    out
}

/// What [`format_into!`] calls: `args` appended to `out`, the caller's buffer, which keeps its
/// allocator and its capacity across calls. Panics as `format!` does when a formatting trait
/// implementation returns an error into a buffer that cannot fail.
#[track_caller]
pub fn format_into<W: fmt::Write + ?Sized>(out: &mut W, args: fmt::Arguments<'_>) {
    if out.write_fmt(args).is_err() {
        panic!(
            "format_into!: a formatting trait implementation returned an error when writing to the caller's buffer"
        );
    }
}

/// Bytes [`String::from_utf8`] refused, given back with where they stop being UTF-8.
pub struct FromUtf8Error<A: Allocator> {
    bytes: Vec<u8, A>,
    error: Utf8Error,
}

impl<A: Allocator> FromUtf8Error<A> {
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    pub fn into_bytes(self) -> Vec<u8, A> {
        self.bytes
    }

    pub fn utf8_error(&self) -> Utf8Error {
        self.error
    }
}

impl<A: Allocator> fmt::Display for FromUtf8Error<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bytes given to String::from_utf8 are not UTF-8: {}", self.bytes.len(), self.error)
    }
}

impl<A: Allocator> fmt::Debug for FromUtf8Error<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FromUtf8Error").field("bytes", &self.bytes.as_slice()).field("error", &self.error).finish()
    }
}

impl<A: Allocator> core::error::Error for FromUtf8Error<A> {}

impl<A: Allocator> Deref for String<A> {
    type Target = str;

    #[inline]
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl<A: Allocator> DerefMut for String<A> {
    #[inline]
    fn deref_mut(&mut self) -> &mut str {
        self.as_mut_str()
    }
}

impl<A: Allocator> AsRef<str> for String<A> {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl<A: Allocator> AsRef<[u8]> for String<A> {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

#[cfg(feature = "std")]
impl<A: Allocator> AsRef<std::ffi::OsStr> for String<A> {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.as_str().as_ref()
    }
}

#[cfg(feature = "std")]
impl<A: Allocator> AsRef<std::path::Path> for String<A> {
    fn as_ref(&self) -> &std::path::Path {
        std::path::Path::new(self.as_str())
    }
}

impl<A: Allocator> Borrow<str> for String<A> {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl<A: Allocator> BorrowMut<str> for String<A> {
    fn borrow_mut(&mut self) -> &mut str {
        self.as_mut_str()
    }
}

impl<A: Allocator> fmt::Write for String<A> {
    #[inline]
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.push_str(text);
        Ok(())
    }

    #[inline]
    fn write_char(&mut self, ch: char) -> fmt::Result {
        self.push(ch);
        Ok(())
    }
}

impl<A: Allocator> fmt::Display for String<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_str(), f)
    }
}

impl<A: Allocator> fmt::Debug for String<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl<A: Allocator + Clone> Clone for String<A> {
    /// Allocates from the collection's own allocator: prefer `clone_in`.
    fn clone(&self) -> String<A> {
        String { bytes: self.bytes.clone() }
    }
}

impl<A: Allocator> String<A> {
    /// A copy of the text in another allocator, room for exactly its length.
    pub fn try_clone_in<B: Allocator>(&self, alloc: B) -> Result<String<B>, AllocFailure> {
        String::try_from_str_in(self.as_str(), alloc)
    }

    /// [`String::try_clone_in`], panicking with the [`AllocFailure`].
    #[track_caller]
    pub fn clone_in<B: Allocator>(&self, alloc: B) -> String<B> {
        String::from_str_in(self.as_str(), alloc)
    }
}

// Equality, order and hash are `str`'s, so `Borrow<str>` holds for maps and sets.

impl<A: Allocator> Hash for String<A> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl<A: Allocator, B: Allocator> PartialEq<String<B>> for String<A> {
    #[inline]
    fn eq(&self, other: &String<B>) -> bool {
        self.as_str() == other.as_str()
    }
}

impl<A: Allocator> Eq for String<A> {}

impl<A: Allocator> PartialOrd for String<A> {
    #[inline]
    fn partial_cmp(&self, other: &String<A>) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<A: Allocator> Ord for String<A> {
    #[inline]
    fn cmp(&self, other: &String<A>) -> Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl<A: Allocator> PartialEq<str> for String<A> {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl<A: Allocator> PartialEq<&str> for String<A> {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl<A: Allocator> PartialEq<String<A>> for str {
    fn eq(&self, other: &String<A>) -> bool {
        self == other.as_str()
    }
}

impl<A: Allocator> PartialEq<String<A>> for &str {
    fn eq(&self, other: &String<A>) -> bool {
        *self == other.as_str()
    }
}

impl<A: Allocator> Extend<char> for String<A> {
    fn extend<I: IntoIterator<Item = char>>(&mut self, iter: I) {
        let iter = iter.into_iter();
        self.reserve(iter.size_hint().0);
        iter.for_each(|ch| self.push(ch));
    }
}

impl<'a, A: Allocator> Extend<&'a char> for String<A> {
    fn extend<I: IntoIterator<Item = &'a char>>(&mut self, iter: I) {
        let iter = iter.into_iter();
        self.reserve(iter.size_hint().0);
        iter.for_each(|&ch| self.push(ch));
    }
}

impl<'a, A: Allocator> Extend<&'a str> for String<A> {
    fn extend<I: IntoIterator<Item = &'a str>>(&mut self, iter: I) {
        iter.into_iter().for_each(|text| self.push_str(text));
    }
}
