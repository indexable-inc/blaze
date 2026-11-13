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

use crate::{Named, OutOfMemory, Vec};

/// A growable UTF-8 string in `A`: std's `String` with an allocator, as `bumpalo`'s
/// `collections::String` is in a `Bump`. Every growth is fallible and names the arena
/// ([`OutOfMemory`]); there is no other form (docs/v2/plan/design/fallible-bstd.md).
pub struct String<A: Allocator> {
    /// UTF-8 whenever a method returns: only `str`s, `char`s and bytes checked by
    /// [`String::from_utf8`] go in, and only whole characters come out.
    bytes: Vec<u8, A>,
}

/// [`String::truncate`] was asked to cut inside a character.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InsideChar {
    /// The byte offset asked for.
    pub at: usize,
    /// The string's length in bytes.
    pub len: usize,
}

impl fmt::Display for InsideChar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "blaze_mem::String::truncate({}): byte {} of the {}-byte string is inside a character",
            self.at, self.at, self.len
        )
    }
}

impl std::error::Error for InsideChar {}

impl<A: Allocator> String<A> {
    /// An empty string in `alloc`; nothing is allocated until it grows.
    #[inline]
    pub fn new_in(alloc: A) -> String<A> {
        String { bytes: Vec::new_in(alloc) }
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

    /// Empties it, keeping its capacity.
    #[inline]
    pub fn clear(&mut self) {
        self.bytes.clear();
    }

    /// Shortens it to `new_len` bytes; nothing when it is no longer. A `new_len` inside a
    /// character is refused, the string as it was.
    pub fn truncate(&mut self, new_len: usize) -> Result<(), InsideChar> {
        if new_len >= self.len() {
            return Ok(());
        }
        if !self.as_str().is_char_boundary(new_len) {
            return Err(InsideChar { at: new_len, len: self.len() });
        }
        self.bytes.truncate(new_len);
        Ok(())
    }

    /// Takes its last character off.
    pub fn pop(&mut self) -> Option<char> {
        let ch = self.as_str().chars().next_back()?;
        // A character of the string is no longer than the string.
        self.bytes.truncate(self.len().saturating_sub(ch.len_utf8()));
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
}

impl<A: Allocator + Named> String<A> {
    /// An empty string in `alloc` with room for `capacity` bytes.
    #[inline]
    pub fn with_capacity_in(capacity: usize, alloc: A) -> Result<String<A>, OutOfMemory> {
        Ok(String { bytes: Vec::with_capacity_in(capacity, alloc)? })
    }

    /// A copy of `text` in `alloc`.
    #[inline]
    pub fn from_str_in(text: &str, alloc: A) -> Result<String<A>, OutOfMemory> {
        Ok(String { bytes: Vec::from_slice_in(text.as_bytes(), alloc)? })
    }

    /// Appends `ch`, or says the arena that had no room; the string is then as it was.
    #[inline]
    pub fn push(&mut self, ch: char) -> Result<(), OutOfMemory> {
        let mut buffer = [0; 4];
        self.bytes.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes())
    }

    /// Appends `text`, or says the arena that had no room; the string is then as it was.
    #[inline]
    pub fn push_str(&mut self, text: &str) -> Result<(), OutOfMemory> {
        self.bytes.extend_from_slice(text.as_bytes())
    }

    /// Room for at least `additional` more bytes.
    #[inline]
    pub fn reserve(&mut self, additional: usize) -> Result<(), OutOfMemory> {
        self.bytes.reserve(additional)
    }

    /// Appends each of `texts`. On an error the texts before it are kept.
    pub fn extend<'t>(
        &mut self,
        texts: impl IntoIterator<Item = &'t str>,
    ) -> Result<(), OutOfMemory> {
        for text in texts {
            self.push_str(text)?;
        }
        Ok(())
    }

    /// Appends `args`, formatted: `write!` into the string, with the arena named when it has
    /// no room. What was written before an error is kept.
    pub fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> Result<(), FormatError<OutOfMemory>> {
        write_to(self, args)
    }

    /// A copy of it in the same allocator.
    pub fn clone_in_place(&self) -> Result<String<A>, OutOfMemory>
    where
        A: Clone,
    {
        String::from_str_in(self.as_str(), self.allocator().clone())
    }
}

/// Where formatted text goes: the one formatting core under [`format_in!`](crate::format_in)
/// (a growing [`String`]) and [`format_into!`](crate::format_into) (a buffer the caller
/// lends), so neither repeats the other.
trait Take {
    /// Why it took no more.
    type Full;

    fn take(&mut self, text: &str) -> Result<(), Self::Full>;
}

impl<A: Allocator + Named> Take for String<A> {
    type Full = OutOfMemory;

    #[inline]
    fn take(&mut self, text: &str) -> Result<(), OutOfMemory> {
        self.push_str(text)
    }
}

/// `core::fmt`'s writer over a [`Take`]: `fmt::Error` carries nothing, so why the taker
/// stopped is kept here for [`write_to`] to return.
struct Sink<'t, T: Take> {
    to: &'t mut T,
    full: Option<T::Full>,
}

impl<T: Take> fmt::Write for Sink<'_, T> {
    #[inline]
    fn write_str(&mut self, text: &str) -> fmt::Result {
        match self.to.take(text) {
            Ok(()) => Ok(()),
            Err(full) => {
                self.full = Some(full);
                Err(fmt::Error)
            }
        }
    }
}

/// The formatting core: `args` written into `to`.
fn write_to<T: Take>(to: &mut T, args: fmt::Arguments<'_>) -> Result<(), FormatError<T::Full>> {
    if let Some(text) = args.as_str() {
        return to.take(text).map_err(FormatError::Full);
    }
    let mut sink = Sink { to, full: None };
    match fmt::Write::write_fmt(&mut sink, args) {
        Ok(()) => Ok(()),
        Err(fmt::Error) => match sink.full {
            Some(full) => Err(FormatError::Full(full)),
            None => Err(FormatError::Trait),
        },
    }
}

/// Why [`format_in!`](crate::format_in), [`format_into!`](crate::format_into) or
/// [`String::write_fmt`] did not finish its text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FormatError<Full> {
    /// Where the text goes had no room: an [`OutOfMemory`] naming the arena of a string, a
    /// [`TooSmall`] saying what a lent buffer needed.
    Full(Full),
    /// A `Display` or `Debug` implementation among the arguments returned an error though
    /// every byte it wrote was taken: that implementation is wrong (`format!` panics here).
    Trait,
}

impl<Full: fmt::Display> fmt::Display for FormatError<Full> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::Full(full) => full.fmt(f),
            FormatError::Trait => f.write_str(
                "a formatting trait implementation returned an error when writing to text \
                 that took every byte it was given",
            ),
        }
    }
}

impl<Full: fmt::Debug + fmt::Display> std::error::Error for FormatError<Full> {}

/// What [`format_in!`](crate::format_in) expands to: `args` written into a new string in
/// `alloc`, or why not: the arena that had no room, or a formatting trait that returned an
/// error of its own.
#[doc(hidden)]
pub fn format<A: Allocator + Named>(
    alloc: A,
    args: fmt::Arguments<'_>,
) -> Result<String<A>, FormatError<OutOfMemory>> {
    let mut out = String::new_in(alloc);
    write_to(&mut out, args)?;
    Ok(out)
}

/// `format!` into the allocator given first: `format_in!(frame, "{name}-{n}")?` is a
/// [`String<&Frame>`](crate::String), as `bumpalo::format!(in bump, ..)` is in a `Bump`. It
/// is a `Result`: an allocator with no room is a [`FormatError::Full`] naming it.
#[macro_export]
macro_rules! format_in {
    ($alloc:expr, $($arg:tt)*) => {
        $crate::text::format($alloc, ::std::format_args!($($arg)*))
    };
}

/// A buffer [`format_into!`](crate::format_into) was lent is too small for its text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TooSmall {
    /// The bytes the whole text takes.
    pub needed: usize,
    /// The bytes the buffer has.
    pub room: usize,
}

impl fmt::Display for TooSmall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "blaze_mem::format_into!: the text takes {} bytes and the buffer lent has {}",
            self.needed, self.room
        )
    }
}

impl std::error::Error for TooSmall {}

/// A lent buffer being filled: it copies what fits and counts all of it, so a buffer that is
/// too small still learns what the text needed.
struct Lent<'b> {
    buffer: &'b mut [u8],
    /// Bytes of the text so far, those that did not fit included.
    needed: usize,
}

impl Take for Lent<'_> {
    type Full = TooSmall;

    #[inline]
    fn take(&mut self, text: &str) -> Result<(), TooSmall> {
        let text = text.as_bytes();
        if let Some(room) = self.buffer.get_mut(self.needed..) {
            // What fits is copied. A text cut short may end inside a character:
            // `format_into` returns no text then, only the count.
            for (slot, byte) in room.iter_mut().zip(text) {
                *slot = *byte;
            }
        }
        self.needed = self.needed.saturating_add(text.len());
        Ok(())
    }
}

/// What [`format_into!`](crate::format_into) expands to: `args` written at the start of
/// `buffer` and lent back as text, allocating nothing; or [`TooSmall`] with the bytes the
/// whole text needs (the buffer then holds a prefix of it, possibly cut inside a character).
#[doc(hidden)]
pub fn format_into<'b>(
    buffer: &'b mut [u8],
    args: fmt::Arguments<'_>,
) -> Result<&'b mut str, FormatError<TooSmall>> {
    let room = buffer.len();
    let mut lent = Lent { buffer, needed: 0 };
    write_to(&mut lent, args)?;
    let Lent { buffer, needed } = lent;
    let Some(written) = buffer.get_mut(..needed) else {
        return Err(FormatError::Full(TooSmall { needed, room }));
    };
    match str::from_utf8_mut(written) {
        Ok(text) => Ok(text),
        // Whole `str`s were copied in order and all of them fit, so the bytes are UTF-8;
        // a formatting core that wrote anything else is refused, not trusted.
        Err(_) => Err(FormatError::Trait),
    }
}

/// `format!` into a buffer the caller lends, allocating nothing:
/// `format_into!(&mut buffer, "{name}-{n}")?` is the text, as a `&mut str` of `buffer`. A
/// buffer that is too small is a [`FormatError::Full`] of [`TooSmall`], which says how many
/// bytes the text needs. For code that may not allocate; [`format_in!`](crate::format_in) is
/// the same formatting core into a string that grows.
#[macro_export]
macro_rules! format_into {
    ($buffer:expr, $($arg:tt)*) => {
        $crate::text::format_into($buffer, ::std::format_args!($($arg)*))
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
