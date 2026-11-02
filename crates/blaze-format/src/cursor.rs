//! The checked little-endian byte cursor, the one implementation: what every hand-written wire,
//! journal, report, blob and object-file reader in blaze reads its fields with, and [`Put`],
//! what their writers append them with.
//!
//! A [`Reader`] holds the whole input and what is left of it. Each read is one length compare
//! and one unaligned little-endian load, and moves past what it read; a read that fails leaves
//! the reader where it was and says, in an [`Error`], at which byte and what it wanted. The
//! error is built out of line (`#[cold]`), so the inlined read is the compare, the load and the
//! move. Lengths and counts read from the bytes are checked against what is left before anything
//! is sliced or allocated for them.
//!
//! A caller with its own error type writes one `impl From<cursor::Error> for ItsError` and uses
//! `?`, or one `map_err` where a field wants its own words. `String` errors get the `From` here,
//! and `&'static str` ones [`Error::as_str`].
//!
//! Safe code only: reading mapped, untrusted bytes in place stays behind the check-then-view
//! rule of docs/v2/plan/design/unsafe.md, and this never makes a view the checks don't cover.
//! docs/v2/plan/lanes/dedup/cursor.md gives the bound.

#![forbid(unsafe_code)]

use std::{fmt, mem::size_of};

use crate::leb::{self, LebError};

/// Reads fields front to back out of `bytes`, every read bounds-checked.
#[derive(Clone, Copy, Debug)]
pub struct Reader<'a> {
    whole: &'a [u8],
    rest: &'a [u8],
}

/// Why a read failed: at which byte the read started, and what it wanted there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Error {
    /// From the start of the reader's bytes.
    pub at: usize,
    pub wanted: Wanted,
}

/// What a failed read wanted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wanted {
    /// `len` bytes, where `left` were.
    Bytes { len: usize, left: usize },
    /// A LEB128, and the bytes were not one.
    Leb(LebError),
    /// `len` bytes of UTF-8, and they were not.
    Utf8 { len: usize },
    /// The NUL ending a string, and none was in the `left` bytes.
    Nul { left: usize },
    /// Room for `count` items, where only `left` bytes were.
    Count { count: u64, left: usize },
    /// The end, where `left` bytes were.
    End { left: usize },
}

const _: () = assert!(size_of::<Error>() <= 32, "an error stays small, so a result of one is cheap");

impl Error {
    /// What went wrong, without where: for a caller whose errors are `&'static str`.
    pub const fn as_str(self) -> &'static str {
        match self.wanted {
            Wanted::Bytes { .. } => "bytes that end early",
            Wanted::Leb(error) => error.as_str(),
            Wanted::Utf8 { .. } => "text that is not UTF-8",
            Wanted::Nul { .. } => "a string without its NUL",
            Wanted::Count { .. } => "a count of more than the bytes left hold",
            Wanted::End { .. } => "bytes past the end",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let at = self.at;
        match self.wanted {
            Wanted::Bytes { len, left } => write!(f, "at byte {at}: {len} bytes wanted, {left} left"),
            Wanted::Leb(error) => write!(f, "at byte {at}: {error}"),
            Wanted::Utf8 { len } => write!(f, "at byte {at}: {len} bytes that are not UTF-8"),
            Wanted::Nul { left } => {
                write!(f, "at byte {at}: a string without its NUL in the {left} bytes left")
            }
            Wanted::Count { count, left } => {
                write!(f, "at byte {at}: a count of {count} with {left} bytes left")
            }
            Wanted::End { left } => write!(f, "at byte {at}: {left} bytes past the end"),
        }
    }
}

impl std::error::Error for Error {}

impl From<Error> for String {
    fn from(error: Error) -> String {
        error.to_string() // alloc: error path
    }
}

impl<'a> Reader<'a> {
    #[inline]
    pub const fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { whole: bytes, rest: bytes }
    }

    /// A reader of `bytes` whose next read is at `at`, counted from the start of `bytes`.
    #[inline]
    pub fn starting_at(bytes: &'a [u8], at: usize) -> Result<Reader<'a>, Error> {
        let mut reader = Reader::new(bytes);
        reader.seek(at)?;
        Ok(reader)
    }

    /// Where the next read is, from the start of the bytes.
    #[inline]
    pub const fn at(&self) -> usize {
        self.whole.len() - self.rest.len()
    }

    /// How many bytes are left.
    #[inline]
    pub const fn left(&self) -> usize {
        self.rest.len()
    }

    /// Whether every byte was read.
    #[inline]
    pub const fn is_done(&self) -> bool {
        self.rest.is_empty()
    }

    /// All the bytes, read or not.
    #[inline]
    pub const fn bytes(&self) -> &'a [u8] {
        self.whole
    }

    /// The bytes left, not moving past them.
    #[inline]
    pub const fn remaining(&self) -> &'a [u8] {
        self.rest
    }

    /// The bytes left, moving past them all.
    #[inline]
    pub fn take_rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.rest)
    }

    /// Moves the next read to `at`, from the start of the bytes; at most their length.
    #[inline]
    pub fn seek(&mut self, at: usize) -> Result<(), Error> {
        match self.whole.get(at..) {
            Some(rest) => {
                self.rest = rest;
                Ok(())
            }
            None => Err(self.past(at)),
        }
    }

    /// Every byte was read; [`Wanted::End`] when some are left.
    #[inline]
    pub fn finish(&self) -> Result<(), Error> {
        match self.rest.len() {
            0 => Ok(()),
            left => Err(self.fail(Wanted::End { left })),
        }
    }

    /// The next `len` bytes.
    #[inline]
    pub fn take(&mut self, len: usize) -> Result<&'a [u8], Error> {
        match self.rest.split_at_checked(len) {
            Some((taken, rest)) => {
                self.rest = rest;
                Ok(taken)
            }
            None => Err(self.short(len)),
        }
    }

    /// Moves past the next `len` bytes.
    #[inline]
    pub fn skip(&mut self, len: usize) -> Result<(), Error> {
        self.take(len).map(drop)
    }

    /// The next `N` bytes, as an array a caller can copy or read in place.
    #[inline]
    pub fn array<const N: usize>(&mut self) -> Result<&'a [u8; N], Error> {
        match self.rest.split_first_chunk::<N>() {
            Some((taken, rest)) => {
                self.rest = rest;
                Ok(taken)
            }
            None => Err(self.short(N)),
        }
    }

    #[inline]
    pub fn u8(&mut self) -> Result<u8, Error> {
        match self.rest.split_first() {
            Some((&byte, rest)) => {
                self.rest = rest;
                Ok(byte)
            }
            None => Err(self.short(1)),
        }
    }

    #[inline]
    pub fn u16(&mut self) -> Result<u16, Error> {
        self.array().map(|it| u16::from_le_bytes(*it))
    }

    #[inline]
    pub fn u32(&mut self) -> Result<u32, Error> {
        self.array().map(|it| u32::from_le_bytes(*it))
    }

    #[inline]
    pub fn u64(&mut self) -> Result<u64, Error> {
        self.array().map(|it| u64::from_le_bytes(*it))
    }

    #[inline]
    pub fn u128(&mut self) -> Result<u128, Error> {
        self.array().map(|it| u128::from_le_bytes(*it))
    }

    #[inline]
    pub fn i8(&mut self) -> Result<i8, Error> {
        self.u8().map(|it| it as i8)
    }

    #[inline]
    pub fn i16(&mut self) -> Result<i16, Error> {
        self.array().map(|it| i16::from_le_bytes(*it))
    }

    #[inline]
    pub fn i32(&mut self) -> Result<i32, Error> {
        self.array().map(|it| i32::from_le_bytes(*it))
    }

    #[inline]
    pub fn i64(&mut self) -> Result<i64, Error> {
        self.array().map(|it| i64::from_le_bytes(*it))
    }

    /// An unsigned LEB128 of at most 64 bits ([`leb::read_u64`]).
    #[inline]
    pub fn uleb(&mut self) -> Result<u64, Error> {
        self.leb(leb::read_u64)
    }

    /// A signed LEB128 of at most 64 bits ([`leb::read_i64`]).
    #[inline]
    pub fn sleb(&mut self) -> Result<i64, Error> {
        self.leb(leb::read_i64)
    }

    /// An unsigned LEB128 of at most 5 bytes and 32 bits, as wasm's `u32` ([`leb::read_u32`]).
    #[inline]
    pub fn uleb32(&mut self) -> Result<u32, Error> {
        self.leb(leb::read_u32)
    }

    /// A signed LEB128 of at most 5 bytes and 32 bits, as wasm's `i32` ([`leb::read_i32`]).
    #[inline]
    pub fn sleb32(&mut self) -> Result<i32, Error> {
        self.leb(leb::read_i32)
    }

    /// Moves past a signed or unsigned LEB128, not decoding it ([`leb::skip`]).
    #[inline]
    pub fn skip_leb(&mut self) -> Result<(), Error> {
        self.leb(leb::skip)
    }

    /// A `u32` length, then that many bytes.
    #[inline]
    pub fn bytes_u32(&mut self) -> Result<&'a [u8], Error> {
        let start = *self;
        let len = self.u32()?;
        self.prefixed(start, len as u64)
    }

    /// A `u64` length, then that many bytes.
    #[inline]
    pub fn bytes_u64(&mut self) -> Result<&'a [u8], Error> {
        let start = *self;
        let len = self.u64()?;
        self.prefixed(start, len)
    }

    /// An unsigned LEB128 length, then that many bytes.
    #[inline]
    pub fn bytes_uleb(&mut self) -> Result<&'a [u8], Error> {
        let start = *self;
        let len = self.uleb()?;
        self.prefixed(start, len)
    }

    /// A `u32` length, then that many bytes of UTF-8.
    #[inline]
    pub fn str_u32(&mut self) -> Result<&'a str, Error> {
        let start = *self;
        let bytes = self.bytes_u32()?;
        self.utf8(start, bytes)
    }

    /// A `u64` length, then that many bytes of UTF-8.
    #[inline]
    pub fn str_u64(&mut self) -> Result<&'a str, Error> {
        let start = *self;
        let bytes = self.bytes_u64()?;
        self.utf8(start, bytes)
    }

    /// An unsigned LEB128 length, then that many bytes of UTF-8.
    #[inline]
    pub fn str_uleb(&mut self) -> Result<&'a str, Error> {
        let start = *self;
        let bytes = self.bytes_uleb()?;
        self.utf8(start, bytes)
    }

    /// The bytes up to the next NUL, moving past the NUL too.
    #[inline]
    pub fn cstr(&mut self) -> Result<&'a [u8], Error> {
        match self.rest.iter().position(|&byte| byte == 0) {
            Some(len) => {
                let taken = &self.rest[..len];
                self.rest = &self.rest[len + 1..];
                Ok(taken)
            }
            None => Err(self.fail(Wanted::Nul { left: self.rest.len() })),
        }
    }

    /// A `u32` count of items that each take at least `each` bytes (at least 1): more than the
    /// bytes left could hold is [`Wanted::Count`], so a count never makes a caller allocate for
    /// bytes that are not there.
    #[inline]
    pub fn count_u32(&mut self, each: usize) -> Result<usize, Error> {
        let start = *self;
        let count = self.u32()?;
        self.counted(start, count as u64, each)
    }

    /// A `u64` count, as [`Reader::count_u32`].
    #[inline]
    pub fn count_u64(&mut self, each: usize) -> Result<usize, Error> {
        let start = *self;
        let count = self.u64()?;
        self.counted(start, count, each)
    }

    /// An unsigned LEB128 count, as [`Reader::count_u32`].
    #[inline]
    pub fn count_uleb(&mut self, each: usize) -> Result<usize, Error> {
        let start = *self;
        let count = self.uleb()?;
        self.counted(start, count, each)
    }

    #[inline]
    fn leb<T>(
        &mut self,
        read: impl FnOnce(&[u8], &mut usize) -> Result<T, LebError>,
    ) -> Result<T, Error> {
        let mut len = 0;
        match read(self.rest, &mut len) {
            Ok(value) => {
                self.rest = &self.rest[len..];
                Ok(value)
            }
            Err(error) => Err(self.fail(Wanted::Leb(error))),
        }
    }

    /// The `len` bytes after a length read from `start`; on an error, back at `start`.
    #[inline]
    fn prefixed(&mut self, start: Reader<'a>, len: u64) -> Result<&'a [u8], Error> {
        let taken = usize::try_from(len).ok().and_then(|len| self.rest.split_at_checked(len));
        match taken {
            Some((taken, rest)) => {
                self.rest = rest;
                Ok(taken)
            }
            None => {
                let len = usize::try_from(len).unwrap_or(usize::MAX);
                Err(self.reset(start, Wanted::Bytes { len, left: self.rest.len() }))
            }
        }
    }

    #[inline]
    fn utf8(&mut self, start: Reader<'a>, bytes: &'a [u8]) -> Result<&'a str, Error> {
        match std::str::from_utf8(bytes) {
            Ok(text) => Ok(text),
            Err(_) => Err(self.reset(start, Wanted::Utf8 { len: bytes.len() })),
        }
    }

    #[inline]
    fn counted(&mut self, start: Reader<'a>, count: u64, each: usize) -> Result<usize, Error> {
        let need = count.checked_mul(each.max(1) as u64);
        if need.is_some_and(|need| need <= self.rest.len() as u64) {
            return Ok(count as usize);
        }
        Err(self.reset(start, Wanted::Count { count, left: self.rest.len() }))
    }

    #[cold]
    #[inline(never)]
    fn short(&self, len: usize) -> Error {
        self.fail(Wanted::Bytes { len, left: self.rest.len() })
    }

    #[cold]
    #[inline(never)]
    fn past(&self, at: usize) -> Error {
        Error { at: self.whole.len(), wanted: Wanted::Bytes { len: at - self.whole.len(), left: 0 } }
    }

    #[cold]
    #[inline(never)]
    fn reset(&mut self, start: Reader<'a>, wanted: Wanted) -> Error {
        let error = self.fail(wanted);
        *self = start;
        error
    }

    #[cold]
    fn fail(&self, wanted: Wanted) -> Error {
        Error { at: self.at(), wanted }
    }
}

/// Appends little-endian fields: what a [`Reader`] reads, written.
pub trait Put {
    fn put_u8(&mut self, value: u8);
    fn put_u16(&mut self, value: u16);
    fn put_u32(&mut self, value: u32);
    fn put_u64(&mut self, value: u64);
    fn put_u128(&mut self, value: u128);
    fn put_i8(&mut self, value: i8);
    fn put_i16(&mut self, value: i16);
    fn put_i32(&mut self, value: i32);
    fn put_i64(&mut self, value: i64);
    /// An unsigned LEB128 ([`leb::write_u64`]).
    fn put_uleb(&mut self, value: u64);
    /// A signed LEB128 ([`leb::write_i64`]).
    fn put_sleb(&mut self, value: i64);
    /// `bytes` as they are, no length.
    fn put_bytes(&mut self, bytes: &[u8]);
    /// A `u32` length, then `bytes`. Panics past `u32::MAX` bytes: the format can't say so.
    fn put_bytes_u32(&mut self, bytes: &[u8]);
    /// A `u64` length, then `bytes`.
    fn put_bytes_u64(&mut self, bytes: &[u8]);
    /// An unsigned LEB128 length, then `bytes`.
    fn put_bytes_uleb(&mut self, bytes: &[u8]);
    /// `bytes`, then a NUL. Panics when `bytes` holds one: [`Reader::cstr`] would stop there.
    fn put_cstr(&mut self, bytes: &[u8]);
    /// Zeros up to the next multiple of `multiple` bytes (not 0) of what is written.
    fn pad_to(&mut self, multiple: usize);
    /// Writes `value` over the 4 bytes at `at`, written before: a length or an offset known
    /// only once what follows it is. Panics past the end.
    fn patch_u32(&mut self, at: usize, value: u32);
    /// [`Put::patch_u32`] of 8 bytes.
    fn patch_u64(&mut self, at: usize, value: u64);
}

impl Put for Vec<u8> {
    #[inline]
    fn put_u8(&mut self, value: u8) {
        self.push(value);
    }

    #[inline]
    fn put_u16(&mut self, value: u16) {
        self.extend_from_slice(&value.to_le_bytes());
    }

    #[inline]
    fn put_u32(&mut self, value: u32) {
        self.extend_from_slice(&value.to_le_bytes());
    }

    #[inline]
    fn put_u64(&mut self, value: u64) {
        self.extend_from_slice(&value.to_le_bytes());
    }

    #[inline]
    fn put_u128(&mut self, value: u128) {
        self.extend_from_slice(&value.to_le_bytes());
    }

    #[inline]
    fn put_i8(&mut self, value: i8) {
        self.push(value as u8);
    }

    #[inline]
    fn put_i16(&mut self, value: i16) {
        self.extend_from_slice(&value.to_le_bytes());
    }

    #[inline]
    fn put_i32(&mut self, value: i32) {
        self.extend_from_slice(&value.to_le_bytes());
    }

    #[inline]
    fn put_i64(&mut self, value: i64) {
        self.extend_from_slice(&value.to_le_bytes());
    }

    #[inline]
    fn put_uleb(&mut self, value: u64) {
        leb::write_u64(self, value);
    }

    #[inline]
    fn put_sleb(&mut self, value: i64) {
        leb::write_i64(self, value);
    }

    #[inline]
    fn put_bytes(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }

    #[inline]
    fn put_bytes_u32(&mut self, bytes: &[u8]) {
        let len = u32::try_from(bytes.len()).expect("at most u32::MAX bytes behind a u32 length");
        self.put_u32(len);
        self.extend_from_slice(bytes);
    }

    #[inline]
    fn put_bytes_u64(&mut self, bytes: &[u8]) {
        self.put_u64(bytes.len() as u64);
        self.extend_from_slice(bytes);
    }

    #[inline]
    fn put_bytes_uleb(&mut self, bytes: &[u8]) {
        leb::write_u64(self, bytes.len() as u64);
        self.extend_from_slice(bytes);
    }

    #[inline]
    fn put_cstr(&mut self, bytes: &[u8]) {
        assert!(!bytes.contains(&0), "a C string holding a NUL");
        self.extend_from_slice(bytes);
        self.push(0);
    }

    #[inline]
    fn pad_to(&mut self, multiple: usize) {
        self.resize(self.len().next_multiple_of(multiple), 0);
    }

    #[inline]
    fn patch_u32(&mut self, at: usize, value: u32) {
        self[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    #[inline]
    fn patch_u64(&mut self, at: usize, value: u64) {
        self[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }
}

#[cfg(test)]
mod tests;
