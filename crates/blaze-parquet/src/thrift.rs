//! Thrift's compact protocol, the subset Parquet's footer and page headers use
//! (https://github.com/apache/thrift/blob/master/doc/specs/thrift-compact-protocol.md): structs
//! of fields, each a header (the id's delta from the last in its low nibble's partner, or the
//! id itself as a zigzag varint) and a value; varints, zigzag, binary as a length and bytes,
//! lists of one element type, booleans in the field header.
//!
//! The encoder appends to the caller's buffer. The decoder reads a slice: every length is held
//! to the bytes left, a varint to ten bytes, a nesting to [`DEPTH`], so a hostile footer is a
//! typed refusal, never a large allocation or a deep recursion.

use core::alloc::Allocator;

use blaze_int::Int;
use blaze_own::Vec;

use crate::ParquetError;

/// The compact protocol's type codes.
pub(crate) const TRUE: u8 = 1;
pub(crate) const FALSE: u8 = 2;
pub(crate) const BYTE: u8 = 3;
pub(crate) const I16: u8 = 4;
pub(crate) const I32: u8 = 5;
pub(crate) const I64: u8 = 6;
pub(crate) const DOUBLE: u8 = 7;
pub(crate) const BINARY: u8 = 8;
pub(crate) const LIST: u8 = 9;
pub(crate) const SET: u8 = 10;
pub(crate) const MAP: u8 = 11;
pub(crate) const STRUCT: u8 = 12;

/// How deep structs and lists may nest in what is read: Parquet's footer nests five deep.
pub(crate) const DEPTH: u32 = 32;

/// Zigzag and LEB128 are blaze-kit's `codec`; an `i32` zigzags as the `i64` it widens to.
pub(crate) use blaze_kit::codec::zigzag;

/// `value` as LEB128 (blaze-kit's `codec`), panicking on a refused allocation as `Vec::push`
/// does.
pub(crate) fn varint<A: Allocator>(out: &mut Vec<u8, A>, value: u64) {
    if let Err(failure) = blaze_kit::codec::write_uleb128(value, out) {
        panic!("blaze_parquet: a thrift varint: {failure}");
    }
}

/// A struct being written: its fields' headers say each id's delta from the last.
pub(crate) struct Struct<'a, A: Allocator> {
    out: &'a mut Vec<u8, A>,
    last: i16,
}

impl<'a, A: Allocator> Struct<'a, A> {
    /// A struct written at the end of `out` (a top-level one: a footer, a page header).
    pub(crate) fn new(out: &'a mut Vec<u8, A>) -> Struct<'a, A> {
        Struct { out, last: 0 }
    }

    fn header(&mut self, id: i16, ty: u8) {
        let delta = id - self.last;
        if (1..=15).contains(&delta) {
            self.out.push(delta.cast_unsigned().truncate_to::<u8>() << 4 | ty);
        } else {
            self.out.push(ty);
            varint(self.out, zigzag(i64::from(id)));
        }
        self.last = id;
    }

    pub(crate) fn bool(&mut self, id: i16, value: bool) {
        self.header(id, if value { TRUE } else { FALSE });
    }

    pub(crate) fn i8(&mut self, id: i16, value: i8) {
        self.header(id, BYTE);
        self.out.push(value.cast_unsigned());
    }

    pub(crate) fn i16(&mut self, id: i16, value: i16) {
        self.header(id, I16);
        varint(self.out, zigzag(i64::from(value)));
    }

    pub(crate) fn i32(&mut self, id: i16, value: i32) {
        self.header(id, I32);
        varint(self.out, zigzag(i64::from(value)));
    }

    pub(crate) fn i64(&mut self, id: i16, value: i64) {
        self.header(id, I64);
        varint(self.out, zigzag(value));
    }

    pub(crate) fn binary(&mut self, id: i16, value: &[u8]) {
        self.header(id, BINARY);
        varint(self.out, value.len().widen_to::<u64>());
        self.out.extend_from_slice(value);
    }

    /// A field of a struct, written by `fields`.
    pub(crate) fn strukt(&mut self, id: i16, fields: impl FnOnce(&mut Struct<'_, A>)) {
        self.header(id, STRUCT);
        let mut inner = Struct { out: &mut *self.out, last: 0 };
        fields(&mut inner);
        inner.end();
    }

    /// A field of an empty struct: a union's arm of no value (`STRING`, `LIST`, `NANOS`).
    pub(crate) fn empty(&mut self, id: i16) {
        self.strukt(id, |_| {});
    }

    /// A field of a list of `len` structs, each written by `each`.
    pub(crate) fn structs<T>(
        &mut self,
        id: i16,
        items: &[T],
        mut each: impl FnMut(&mut Struct<'_, A>, &T),
    ) {
        self.list_head(id, STRUCT, items.len());
        for item in items {
            let mut inner = Struct { out: &mut *self.out, last: 0 };
            each(&mut inner, item);
            inner.end();
        }
    }

    /// A field of a list of `len` structs, each written by `each` alike.
    pub(crate) fn repeated(&mut self, id: i16, len: usize, mut each: impl FnMut(&mut Struct<'_, A>)) {
        self.list_head(id, STRUCT, len);
        for _ in 0..len {
            let mut inner = Struct { out: &mut *self.out, last: 0 };
            each(&mut inner);
            inner.end();
        }
    }

    pub(crate) fn i32s(&mut self, id: i16, items: impl ExactSizeIterator<Item = i32>) {
        self.list_head(id, I32, items.len());
        for item in items {
            varint(self.out, zigzag(i64::from(item)));
        }
    }

    pub(crate) fn binaries<'b>(&mut self, id: i16, items: impl ExactSizeIterator<Item = &'b [u8]>) {
        self.list_head(id, BINARY, items.len());
        for item in items {
            varint(self.out, item.len().widen_to::<u64>());
            self.out.extend_from_slice(item);
        }
    }

    fn list_head(&mut self, id: i16, ty: u8, len: usize) {
        self.header(id, LIST);
        match u8::try_from(len) {
            Ok(short @ 0..15) => self.out.push(short << 4 | ty),
            _ => {
                self.out.push(0xF0 | ty);
                varint(self.out, len.widen_to::<u64>());
            }
        }
    }

    /// The struct's end (a stop byte).
    pub(crate) fn end(self) {
        self.out.push(0);
    }
}

/// Compact-protocol bytes being read.
pub(crate) struct In<'a> {
    bytes: &'a [u8],
    at: usize,
    depth: u32,
}

impl<'a> In<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> In<'a> {
        In { bytes, at: 0, depth: 0 }
    }

    /// How many bytes were read.
    pub(crate) fn read(&self) -> usize {
        self.at
    }

    fn left(&self) -> usize {
        self.bytes.len() - self.at
    }

    fn bad(&self, what: &'static str) -> ParquetError {
        ParquetError::Thrift { what, at: self.at }
    }

    pub(crate) fn byte(&mut self) -> Result<u8, ParquetError> {
        let byte = *self.bytes.get(self.at).ok_or(ParquetError::Truncated { what: "thrift", at: self.at })?;
        self.at += 1;
        Ok(byte)
    }

    /// A LEB128 value (blaze-kit's `codec`); an overlong one is named at its first byte.
    pub(crate) fn varint(&mut self) -> Result<u64, ParquetError> {
        blaze_kit::codec::read_uleb128(self.bytes, &mut self.at, 0).map_err(|error| match error {
            blaze_kit::codec::CodecError::Truncated { .. } => ParquetError::Truncated { what: "thrift", at: self.bytes.len() },
            _ => self.bad("a varint past 64 bits"),
        })
    }

    pub(crate) fn i64(&mut self) -> Result<i64, ParquetError> {
        Ok(blaze_kit::codec::unzigzag(self.varint()?))
    }

    pub(crate) fn i32(&mut self) -> Result<i32, ParquetError> {
        let value = self.i64()?;
        i32::try_from(value).map_err(|_| self.bad("an i32 out of range"))
    }

    pub(crate) fn i16(&mut self) -> Result<i16, ParquetError> {
        let value = self.i64()?;
        i16::try_from(value).map_err(|_| self.bad("an i16 out of range"))
    }

    pub(crate) fn i8(&mut self) -> Result<i8, ParquetError> {
        Ok(self.byte()?.cast_signed())
    }

    pub(crate) fn binary(&mut self) -> Result<&'a [u8], ParquetError> {
        let len = self.varint()?;
        let len = usize::try_from(len).map_err(|_| self.bad("a binary longer than memory"))?;
        if len > self.left() {
            return Err(ParquetError::Truncated { what: "a thrift binary", at: self.at });
        }
        let out = &self.bytes[self.at..self.at + len];
        self.at += len;
        Ok(out)
    }

    pub(crate) fn string(&mut self) -> Result<&'a str, ParquetError> {
        let at = self.at;
        core::str::from_utf8(self.binary()?).map_err(|_| ParquetError::Thrift { what: "a string not UTF-8", at })
    }

    /// The next field's id and type, after `last`'s; `None` at the struct's end.
    pub(crate) fn field(&mut self, last: &mut i16) -> Result<Option<(i16, u8)>, ParquetError> {
        let byte = self.byte()?;
        if byte == 0 {
            return Ok(None);
        }
        let ty = byte & 0x0F;
        let delta = byte >> 4;
        let id = if delta == 0 {
            self.i16()?
        } else {
            last.checked_add(i16::from(delta)).ok_or_else(|| self.bad("a field id past i16"))?
        };
        *last = id;
        Ok(Some((id, ty)))
    }

    /// A list's element type and length, the length held to the bytes left (every element
    /// takes at least one).
    pub(crate) fn list_head(&mut self) -> Result<(u8, usize), ParquetError> {
        let byte = self.byte()?;
        let ty = byte & 0x0F;
        let len = match byte >> 4 {
            15 => usize::try_from(self.varint()?).map_err(|_| self.bad("a list longer than memory"))?,
            short => usize::from(short),
        };
        if len > self.left() {
            return Err(ParquetError::Truncated { what: "a thrift list", at: self.at });
        }
        Ok((ty, len))
    }

    /// A boolean field's value, from its header's type.
    pub(crate) fn bool_of(&self, ty: u8) -> Result<bool, ParquetError> {
        match ty {
            TRUE => Ok(true),
            FALSE => Ok(false),
            _ => Err(self.bad("a bool field of another type")),
        }
    }

    /// Into a nested struct or list: refused past [`DEPTH`].
    pub(crate) fn enter(&mut self) -> Result<(), ParquetError> {
        self.depth += 1;
        if self.depth > DEPTH {
            return Err(self.bad("structs nested too deep"));
        }
        Ok(())
    }

    pub(crate) fn leave(&mut self) {
        self.depth -= 1;
    }

    /// Past a value of type `ty` this reader has no use for.
    pub(crate) fn skip(&mut self, ty: u8) -> Result<(), ParquetError> {
        match ty {
            TRUE | FALSE => Ok(()),
            BYTE => self.byte().map(drop),
            I16 | I32 | I64 => self.varint().map(drop),
            DOUBLE => {
                if self.left() < 8 {
                    return Err(ParquetError::Truncated { what: "a thrift double", at: self.at });
                }
                self.at += 8;
                Ok(())
            }
            BINARY => self.binary().map(drop),
            LIST | SET => {
                self.enter()?;
                let (elem, len) = self.list_head()?;
                for _ in 0..len {
                    self.skip_element(elem)?;
                }
                self.leave();
                Ok(())
            }
            MAP => {
                self.enter()?;
                let len = usize::try_from(self.varint()?).map_err(|_| self.bad("a map longer than memory"))?;
                if len > 0 {
                    if len > self.left() {
                        return Err(ParquetError::Truncated { what: "a thrift map", at: self.at });
                    }
                    let types = self.byte()?;
                    for _ in 0..len {
                        self.skip_element(types >> 4)?;
                        self.skip_element(types & 0x0F)?;
                    }
                }
                self.leave();
                Ok(())
            }
            STRUCT => {
                self.enter()?;
                let mut last = 0;
                while let Some((_, ty)) = self.field(&mut last)? {
                    self.skip(ty)?;
                }
                self.leave();
                Ok(())
            }
            _ => Err(self.bad("an unknown thrift type")),
        }
    }

    /// An element of a list: a boolean is a byte of its own there.
    fn skip_element(&mut self, ty: u8) -> Result<(), ParquetError> {
        match ty {
            TRUE | FALSE => self.byte().map(drop),
            _ => self.skip(ty),
        }
    }

    /// The fields of a struct, each given to `field` (its id and type), which reads its value
    /// or says `false` to skip it.
    pub(crate) fn strukt(
        &mut self,
        mut field: impl FnMut(&mut In<'a>, i16, u8) -> Result<bool, ParquetError>,
    ) -> Result<(), ParquetError> {
        self.enter()?;
        let mut last = 0;
        while let Some((id, ty)) = self.field(&mut last)? {
            if !field(self, id, ty)? {
                self.skip(ty)?;
            }
        }
        self.leave();
        Ok(())
    }

    /// Refused unless `ty` is `want`: a field of a known id and another type.
    pub(crate) fn expect(&self, ty: u8, want: u8, what: &'static str) -> Result<(), ParquetError> {
        if ty == want { Ok(()) } else { Err(self.bad(what)) }
    }

    /// A list of `want` elements, each read by `each`.
    pub(crate) fn list(
        &mut self,
        want: u8,
        what: &'static str,
        mut each: impl FnMut(&mut In<'a>) -> Result<(), ParquetError>,
    ) -> Result<(), ParquetError> {
        self.enter()?;
        let (ty, len) = self.list_head()?;
        if len > 0 && ty != want {
            return Err(self.bad(what));
        }
        for _ in 0..len {
            each(self)?;
        }
        self.leave();
        Ok(())
    }
}
