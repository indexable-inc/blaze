//! The spawn seam as records (READINESS.md section 1a, pieces S1, S2 and the type of S5).
//!
//! A build step's spawned command is a [`SpawnRequest`]: the step's key (which seeds the guest's
//! clock and randomness), the guest's personality and declared machine, the identity of its
//! userland, the program and the further tools it may start as pinned bytes, its arguments, its
//! WHOLE environment sorted, its guest working folder, its stdin, its read grant by content, its
//! writable folders and the files already in them. Nothing in it names the host: no host path, no
//! time, no process id. Its key is the BLAKE3 of one canonical encoding, so two builders of one
//! step ask the same question in the same bytes (rules 21, 25).
//!
//! The sandbox's reply is a [`SpawnAnswer`]: status, stdout, stderr, the files written or removed
//! sorted by path, and the read list (`Read`, `Miss`, `List`) sorted and once. Both records have
//! one encoding each, and decoding refuses anything not canonical: a record is canonical or it is
//! refused, never repaired.
//!
//! The strategy that answers is an [`Executor`] value chosen once per command by its setting
//! (rule 78); [`Off`] is the one used when no sandbox runs on this machine, and it refuses every
//! spawn by name ([`SpawnRefused::NoSandbox`]): a spawned command never runs natively (rule 3).

#![no_std]

#[cfg(test)]
extern crate std;

pub mod answer;
/// The digests a request and an answer hold: one home for the proc-macro route and the sandbox.
pub mod digest;
pub mod executor;
pub mod request;

#[cfg(test)]
mod tests;

pub use answer::{AnswerError, Seen, SpawnAnswer, Status, Written};
pub use digest::{TreeError, file_digest, tree_digest};
pub use executor::{Executor, Files, NoSandbox, Off, SpawnRefused};
pub use request::{
    Input, MACHINE_V1, Machine, Personality, RequestError, SpawnRequest, Tool, fixed_env,
};

use core::fmt;

use blaze_ids::ContentId;
use blaze_own::{AString, AVec, Alloc};

/// Why bytes are not a canonical record. Every variant names where the reader stopped or the
/// field it refused; decoding never guesses past one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Malformed {
    /// The first eight bytes are not the record's layout word (they are given, little-endian).
    Layout(u64),
    /// The bytes end inside a field that starts at this offset.
    EndsAt(usize),
    /// A string field is not UTF-8.
    NotUtf8 { field: &'static str },
    /// A list that must be strictly sorted (and so once each) is not.
    Unsorted { field: &'static str },
    /// This many bytes follow the record's last field.
    Trailing(usize),
    /// A field holds a value no canonical record holds: a tag outside its enum, a path that is
    /// not absolute or holds `..` or a NUL, an environment without the fixed entries. Added by the
    /// writer beside the planner's five variants: a form not handled is refused by name (rule 3).
    NotCanonical { field: &'static str },
}

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Malformed::Layout(word) => {
                write!(f, "a spawn record starts with layout word {word:#018x}, not one blaze-spawn writes")
            }
            Malformed::EndsAt(at) => write!(f, "a spawn record ends inside its field at byte {at}"),
            Malformed::NotUtf8 { field } => write!(f, "a spawn record's `{field}` is not UTF-8"),
            Malformed::Unsorted { field } => {
                write!(f, "a spawn record's `{field}` is not sorted once by its key: a record is canonical or refused")
            }
            Malformed::Trailing(n) => write!(f, "a spawn record has {n} bytes after its last field"),
            Malformed::NotCanonical { field } => {
                write!(f, "a spawn record's `{field}` holds a value no canonical record holds")
            }
        }
    }
}

impl core::error::Error for Malformed {}

/// The writer half of the one encoding: numbers little-endian, a string or byte list as a `u32`
/// length then its bytes, a list as a `u32` count then its items, a content id as its 32 bytes.
pub(crate) struct Put<'o, 'a> {
    pub(crate) out: &'o mut AVec<'a, u8>,
}

impl Put<'_, '_> {
    pub(crate) fn u8(&mut self, value: u8) {
        self.out.push(value);
    }

    pub(crate) fn u16(&mut self, value: u16) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn i32(&mut self, value: i32) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn u64(&mut self, value: u64) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn id(&mut self, id: &ContentId) {
        self.out.extend_from_slice(&id.0);
    }

    pub(crate) fn raw32(&mut self, bytes: &[u8; 32]) {
        self.out.extend_from_slice(bytes);
    }

    /// A list's count. A record with more than `u32::MAX` items in one list is not a request any
    /// step makes; it stops here naming the field rather than writing a wrong count.
    pub(crate) fn count(&mut self, len: usize, field: &'static str) {
        let Ok(n) = u32::try_from(len) else {
            panic!("blaze-spawn: the record's `{field}` has {len} items, more than a u32 counts");
        };
        self.u32(n);
    }

    pub(crate) fn bytes(&mut self, bytes: &[u8], field: &'static str) {
        self.count(bytes.len(), field);
        self.out.extend_from_slice(bytes);
    }

    pub(crate) fn str(&mut self, text: &str, field: &'static str) {
        self.bytes(text.as_bytes(), field);
    }
}

/// The reader half: every read names the offset it started at when the bytes run out.
pub(crate) struct Take<'b> {
    pub(crate) bytes: &'b [u8],
    pub(crate) at: usize,
}

impl<'b> Take<'b> {
    pub(crate) fn new(bytes: &'b [u8]) -> Take<'b> {
        Take { bytes, at: 0 }
    }

    pub(crate) fn slice(&mut self, len: usize) -> Result<&'b [u8], Malformed> {
        let start = self.at;
        let Some(end) = start.checked_add(len) else {
            return Err(Malformed::EndsAt(start));
        };
        let Some(slice) = self.bytes.get(start..end) else {
            return Err(Malformed::EndsAt(start));
        };
        self.at = end;
        Ok(slice)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], Malformed> {
        let start = self.at;
        let slice = self.slice(N)?;
        let Ok(array) = <[u8; N]>::try_from(slice) else {
            return Err(Malformed::EndsAt(start));
        };
        Ok(array)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, Malformed> {
        Ok(self.array::<1>()?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, Malformed> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, Malformed> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub(crate) fn i32(&mut self) -> Result<i32, Malformed> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, Malformed> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    pub(crate) fn id(&mut self) -> Result<ContentId, Malformed> {
        Ok(ContentId(self.array()?))
    }

    /// A list's count. Every item of every list takes at least four bytes, so a hostile count
    /// ends in `EndsAt` after at most a quarter of the remaining bytes' items: nothing is
    /// reserved by the count itself.
    pub(crate) fn count(&mut self) -> Result<u32, Malformed> {
        self.u32()
    }

    pub(crate) fn bytes(&mut self) -> Result<&'b [u8], Malformed> {
        let len = self.u32()?;
        self.slice(len as usize)
    }

    pub(crate) fn str(&mut self, field: &'static str) -> Result<&'b str, Malformed> {
        let bytes = self.bytes()?;
        core::str::from_utf8(bytes).map_err(|_| Malformed::NotUtf8 { field })
    }

    pub(crate) fn string<'a>(&mut self, field: &'static str, alloc: Alloc<'a>) -> Result<AString<'a>, Malformed> {
        Ok(AString::from_str_in(self.str(field)?, alloc))
    }

    pub(crate) fn byte_vec<'a>(&mut self, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, Malformed> {
        Ok(AVec::from_slice_in(self.bytes()?, alloc))
    }

    /// The end: anything after the last field is refused.
    pub(crate) fn end(&self) -> Result<(), Malformed> {
        let left = self.bytes.len() - self.at;
        if left != 0 {
            return Err(Malformed::Trailing(left));
        }
        Ok(())
    }
}

/// A guest path as every record holds it: absolute, no `..` component, no NUL.
pub(crate) fn guest_path_ok(path: &str) -> bool {
    path.starts_with('/') && !path.contains('\0') && !path.split('/').any(|part| part == "..")
}
