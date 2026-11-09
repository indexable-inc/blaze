//! Owned text in an allocator the caller names (AGENTS.md rule 11, bump-allocate): [`String`],
//! and on unix [`OsString`] and [`PathBuf`], each over a `Vec<u8, A>`, so a task's text lives
//! in its [`Frame`](crate::Frame) (`String<&Frame>`) or its request's
//! [`ArenaLocal`](crate::ArenaLocal) rather than on the global heap. std's `String`,
//! `OsString` and `PathBuf` take no allocator, and nightly's `allocator_api` gives `Vec` and
//! `Box` one but not them. [`format_in!`](crate::format_in) is `format!` into an allocator;
//! [`frame::string`](crate::frame::string), [`frame::path`](crate::frame::path) and
//! [`frame::os_string`](crate::frame::os_string) make one in a frame.
//!
//! None of them has a default allocator: every place that makes one says where its bytes go.
//!
//! Each compares, orders and hashes as its borrowed form (`str`, `OsStr`, `Path`) does, and
//! borrows as it, so it is a map key looked up by `&str`, `&OsStr` or `&Path`.
//!
//! [`OsString`] and [`PathBuf`] are unix only. There an `OsStr` is any bytes
//! (`std::os::unix::ffi::OsStrExt`), so both are safe code over the bytes. On Windows an
//! `OsStr` is WTF-8, whose only public constructor from bytes,
//! `OsStr::from_encoded_bytes_unchecked`, accepts bytes `as_encoded_bytes` gave or split at a
//! UTF-8 boundary, but not two such strings joined (a lead surrogate at the end of one and a
//! trail surrogate at the start of the other must merge into one code point, which only std's
//! internal `Wtf8Buf::push` does). So on Windows the types are not there, and naming one is a
//! compile error ("cannot find type `PathBuf` in crate `blaze_mem`"), never a silent fallback to
//! the global heap. WASI is left out too: its `OsStrExt` is unstable (`wasi_ext`).

#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub use unix::{OsString, PathBuf};

use std::{
    alloc::Allocator,
    borrow::{Borrow, BorrowMut},
    cmp::Ordering,
    ffi::OsStr,
    fmt,
    hash::{Hash, Hasher},
    ops::{Deref, DerefMut},
    path::Path,
    str::{self, Utf8Error},
};

/// A growable UTF-8 string in `A`: std's `String` with an allocator, as `bumpalo`'s
/// `collections::String` is in a `Bump`.
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
        match str::from_utf8(&bytes) {
            Ok(_) => Ok(String { bytes }),
            Err(error) => Err(FromUtf8Error { bytes, error }),
        }
    }

    #[inline]
    pub fn push(&mut self, ch: char) {
        let mut buffer = [0; 4];
        self.bytes.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
    }

    #[inline]
    pub fn push_str(&mut self, text: &str) {
        self.bytes.extend_from_slice(text.as_bytes());
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        // SAFETY: `bytes` is UTF-8 (the field's invariant).
        unsafe { str::from_utf8_unchecked(&self.bytes) }
    }

    #[inline]
    pub fn as_mut_str(&mut self) -> &mut str {
        // SAFETY: `bytes` is UTF-8 (the field's invariant), and a `&mut str` can only be changed
        // in ways that keep it so.
        unsafe { str::from_utf8_unchecked_mut(&mut self.bytes) }
    }

    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
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
                "blaze_mem::String::truncate({new_len}): byte {new_len} of the {}-byte string is inside a character",
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
    pub fn into_boxed_str(self) -> Box<str, A> {
        let (raw, alloc) = Box::into_raw_with_allocator(self.bytes.into_boxed_slice());
        // SAFETY: `raw` and `alloc` are a `Box<[u8], A>` taken apart just above, whose bytes are
        // UTF-8 (the field's invariant); `str` has `[u8]`'s layout and metadata, so the cast
        // keeps the length and the box frees the same layout from the same allocator.
        unsafe { Box::from_raw_in(raw as *mut str, alloc) }
    }
}

/// What [`format_in!`](crate::format_in) expands to: `args` written into a new string in
/// `alloc`.
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
    let mut out = String::new_in(alloc);
    if fmt::Write::write_fmt(&mut out, args).is_err() {
        panic!(
            "blaze_mem::format_in!: a formatting trait implementation returned an error when writing to a string, which never fails"
        );
    }
    out
}

/// `format!` into the allocator given first: `format_in!(frame, "{name}-{n}")` is a
/// [`String<&Frame>`](crate::String), as `bumpalo::format!(in bump, ..)` is in a `Bump`.
#[macro_export]
macro_rules! format_in {
    ($alloc:expr, $($arg:tt)*) => {
        $crate::text::format($alloc, ::std::format_args!($($arg)*))
    };
}

/// Bytes [`String::from_utf8`] refused, given back with where they stop being UTF-8.
pub struct FromUtf8Error<A: Allocator> {
    bytes: Vec<u8, A>,
    error: Utf8Error,
}

impl<A: Allocator> FromUtf8Error<A> {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
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
        write!(
            f,
            "{} bytes given to blaze_mem::String::from_utf8 are not UTF-8: {}",
            self.bytes.len(),
            self.error
        )
    }
}

impl<A: Allocator> fmt::Debug for FromUtf8Error<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FromUtf8Error")
            .field("bytes", &self.bytes.as_slice())
            .field("error", &self.error)
            .finish()
    }
}

impl<A: Allocator> std::error::Error for FromUtf8Error<A> {}

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

impl<A: Allocator> AsRef<OsStr> for String<A> {
    fn as_ref(&self) -> &OsStr {
        self.as_str().as_ref()
    }
}

impl<A: Allocator> AsRef<Path> for String<A> {
    fn as_ref(&self) -> &Path {
        Path::new(self.as_str())
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
    fn clone(&self) -> String<A> {
        String { bytes: self.bytes.clone() }
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
        self.extend(iter.into_iter().copied());
    }
}

impl<'a, A: Allocator> Extend<&'a str> for String<A> {
    fn extend<I: IntoIterator<Item = &'a str>>(&mut self, iter: I) {
        iter.into_iter().for_each(|text| self.push_str(text));
    }
}
