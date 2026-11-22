//! The byte form of the drive's own records: little-endian fixed-width fields, lengths before
//! lists, written once and read in place. Deterministic: no field holds the machine, a time or a
//! map's order (rule 25); every list is sorted by its writer.

use core::fmt;

use blaze_id::ContentId;
use blaze_own::{AVec, Alloc, Vec};

use crate::condition::{Condition, Var};
use crate::key::{Key, Kind, Name};

/// Why bytes are not a record of the drive's.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct WireError {
    /// Which record form was being read (`"answer page"`, `"certificate"`).
    pub what: &'static str,
    /// The byte where it stopped.
    pub at: u32,
    pub why: WireWhy,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum WireWhy {
    /// Fewer bytes than the field needs.
    Short,
    /// The first four bytes are not this form's.
    Magic,
    /// A byte that is no task kind.
    Kind(u8),
    /// A tag byte this form does not have.
    Tag(u8),
    /// A count larger than the bytes left could hold.
    Count(u32),
    /// Bytes after the record's end.
    Trailing,
    /// A list its writer sorts that is not sorted.
    Unsorted,
    /// Text that is not UTF-8.
    Utf8,
    /// A padding byte that is not zero.
    Pad,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a stored {} is not one at byte {}: {:?} (rule 3: refused, not read past)", self.what, self.at, self.why)
    }
}

impl core::error::Error for WireError {}

/// A record being written, in the caller's allocator.
pub(crate) struct Writer<'a> {
    bytes: AVec<'a, u8>,
}

impl<'a> Writer<'a> {
    pub(crate) fn new_in(magic: &[u8; 4], alloc: Alloc<'a>) -> Writer<'a> {
        let mut bytes = Vec::new_in(alloc);
        bytes.extend_from_slice(magic);
        Writer { bytes }
    }

    pub(crate) fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    pub(crate) fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    /// A count of a list: a list longer than `u32::MAX` is no record anyone writes.
    pub(crate) fn count(&mut self, len: usize) {
        self.u32(u32::try_from(len).expect("a record's list fits u32 (the drive's tables are capped below it)"));
    }

    pub(crate) fn id(&mut self, id: &ContentId) {
        self.bytes.extend_from_slice(&id.0);
    }

    pub(crate) fn key(&mut self, key: Key) {
        self.id(&key.0);
    }

    pub(crate) fn name(&mut self, name: Name) {
        self.u8(name.kind as u8);
        self.id(&name.subject);
    }

    pub(crate) fn bytes(&mut self, bytes: &[u8]) {
        self.count(bytes.len());
        self.bytes.extend_from_slice(bytes);
    }

    /// Bytes whose length was written before them.
    pub(crate) fn raw(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    /// Zero bytes up to the next multiple of `align` from the record's start: a payload that is a
    /// blaze-format buffer starts aligned in a store buffer aligned the same.
    pub(crate) fn pad_to(&mut self, align: usize) {
        while self.bytes.len() % align != 0 {
            self.bytes.push(0);
        }
    }

    pub(crate) fn finish(self) -> AVec<'a, u8> {
        self.bytes
    }

    /// A condition: its atom count, then each (variable, value), sorted.
    pub(crate) fn condition(&mut self, condition: &Condition) {
        self.u8(condition.atoms().len() as u8);
        for (var, value) in condition.atoms() {
            self.u16(*var);
            self.u16(*value);
        }
    }

    /// A split variable or none: `u32::MAX` for none.
    pub(crate) fn split(&mut self, split: Option<Var>) {
        self.u32(split.map_or(u32::MAX, u32::from));
    }
}

/// A record being read in place.
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
    what: &'static str,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8], magic: &[u8; 4], what: &'static str) -> Result<Reader<'a>, WireError> {
        let mut reader = Reader { bytes, at: 0, what };
        match reader.take(4)? == magic {
            true => Ok(reader),
            false => Err(reader.error(WireWhy::Magic)),
        }
    }

    /// A reader of bytes with no magic: a list's entries, already checked by their record.
    pub(crate) fn raw(bytes: &'a [u8], what: &'static str) -> Reader<'a> {
        Reader { bytes, at: 0, what }
    }

    /// The byte it will read next.
    pub(crate) fn position(&self) -> usize {
        self.at
    }

    pub(crate) fn error(&self, why: WireWhy) -> WireError {
        WireError { what: self.what, at: u32::try_from(self.at).unwrap_or(u32::MAX), why }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        let end = self.at.checked_add(n).filter(|end| *end <= self.bytes.len()).ok_or(self.error(WireWhy::Short))?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, WireError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, WireError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// A count of a list whose entries are at least `least` bytes each: refused when the bytes
    /// left cannot hold it, so a corrupt count never sizes an allocation.
    pub(crate) fn count(&mut self, least: usize) -> Result<usize, WireError> {
        let n = self.u32()?;
        let left = self.bytes.len() - self.at;
        match (n as usize).checked_mul(least.max(1)).is_some_and(|need| need <= left) {
            true => Ok(n as usize),
            false => Err(self.error(WireWhy::Count(n))),
        }
    }

    pub(crate) fn id(&mut self) -> Result<ContentId, WireError> {
        let b = self.take(32)?;
        let mut id = [0; 32];
        id.copy_from_slice(b);
        Ok(ContentId(id))
    }

    pub(crate) fn key(&mut self) -> Result<Key, WireError> {
        Ok(Key(self.id()?))
    }

    pub(crate) fn name(&mut self) -> Result<Name, WireError> {
        let byte = self.u8()?;
        let kind = Kind::from_u8(byte).ok_or(self.error(WireWhy::Kind(byte)))?;
        Ok(Name { kind, subject: self.id()? })
    }

    pub(crate) fn bytes(&mut self) -> Result<&'a [u8], WireError> {
        let n = self.count(1)?;
        self.take(n)
    }

    /// `n` bytes whose length was read before them.
    pub(crate) fn take_bytes(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        self.take(n)
    }

    /// The zero bytes [`Writer::pad_to`] wrote; a nonzero one is refused (one encoding).
    pub(crate) fn pad_to(&mut self, align: usize) -> Result<(), WireError> {
        while self.at % align != 0 {
            if self.u8()? != 0 {
                return Err(self.error(WireWhy::Pad));
            }
        }
        Ok(())
    }

    /// A condition [`Writer::condition`] wrote: sorted, no variable twice, at most
    /// [`MOST_ATOMS`](crate::condition::MOST_ATOMS) atoms, or refused (one encoding).
    pub(crate) fn condition(&mut self) -> Result<Condition, WireError> {
        let n = self.u8()?;
        let mut condition = Condition::ANY;
        let mut last: Option<Var> = None;
        for _ in 0..n {
            let (var, value) = (self.u16()?, self.u16()?);
            if last.is_some_and(|l| l >= var) {
                return Err(self.error(WireWhy::Unsorted));
            }
            last = Some(var);
            condition = match condition.with(var, value) {
                Ok(Some(next)) => next,
                _ => return Err(self.error(WireWhy::Count(u32::from(n)))),
            };
        }
        Ok(condition)
    }

    pub(crate) fn split(&mut self) -> Result<Option<Var>, WireError> {
        match self.u32()? {
            u32::MAX => Ok(None),
            var => Var::try_from(var).map(Some).map_err(|_| self.error(WireWhy::Tag(0xff))),
        }
    }

    pub(crate) fn done(&self) -> Result<(), WireError> {
        match self.at == self.bytes.len() {
            true => Ok(()),
            false => Err(self.error(WireWhy::Trailing)),
        }
    }
}

/// Bytes of a name on the wire.
pub(crate) const NAME_BYTES: usize = 33;
