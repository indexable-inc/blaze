//! Flatbuffers, the few forms Arrow's messages use: tables of scalars and offsets, vectors of
//! offsets and of structs, strings.
//!
//! [`Builder`] writes back to front as flatbuffers does (a child before its parent, so every
//! offset points forward), into a buffer kept reversed: a position is a distance from the end,
//! and [`Builder::finish`] turns it around once. [`Tab`] reads a table in place, every offset
//! and length held to the buffer.

use core::alloc::Allocator;

use blaze_int::Int;
use blaze_own::Vec;

use crate::IpcError;

/// A table's fields, by slot, as they are added.
pub(crate) struct TableStart {
    start: usize,
    /// Each slot's field's position; 0 where the table has none.
    slots: [usize; 8],
    top: usize,
}

pub(crate) struct Builder<A: Allocator> {
    /// The bytes so far, last byte first.
    rev: Vec<u8, A>,
}

impl<A: Allocator> Builder<A> {
    pub(crate) fn new_in(alloc: A) -> Builder<A> {
        Builder { rev: Vec::new_in(alloc) }
    }

    /// Zeros until `extra` more bytes leave the position a multiple of `align`.
    fn pad(&mut self, align: usize, extra: usize) {
        while (self.rev.len() + extra) % align != 0 {
            self.rev.push(0);
        }
    }

    /// `bytes`, in their order, before what is there.
    fn raw(&mut self, bytes: &[u8]) {
        for byte in bytes.iter().rev() {
            self.rev.push(*byte);
        }
    }

    /// A scalar of `N` bytes, aligned to `N`: its position.
    fn scalar<const N: usize>(&mut self, le: [u8; N]) -> usize {
        self.pad(N, 0);
        self.raw(&le);
        self.rev.len()
    }

    /// An offset to what is at `target`: its position.
    fn uoffset(&mut self, target: usize) -> usize {
        self.pad(4, 0);
        let at = self.rev.len() + 4;
        let distance = u32::try_from(at - target).unwrap_or(u32::MAX);
        self.raw(&distance.to_le_bytes());
        at
    }

    fn length(&mut self, len: usize) -> usize {
        self.scalar(u32::try_from(len).unwrap_or(u32::MAX).to_le_bytes())
    }

    pub(crate) fn string(&mut self, text: &str) -> usize {
        self.pad(4, text.len() + 1);
        self.rev.push(0);
        self.raw(text.as_bytes());
        self.length(text.len())
    }

    /// A vector of offsets to `targets`.
    pub(crate) fn offsets(&mut self, targets: &[usize]) -> usize {
        self.pad(4, 4 * targets.len());
        for target in targets.iter().rev() {
            self.uoffset(*target);
        }
        self.length(targets.len())
    }

    /// A vector of `count` structs of 8-aligned fields, `bytes` in their order.
    pub(crate) fn structs(&mut self, bytes: &[u8], count: usize) -> usize {
        self.pad(8, bytes.len());
        self.raw(bytes);
        self.length(count)
    }

    pub(crate) fn start(&self) -> TableStart {
        TableStart { start: self.rev.len(), slots: [0; 8], top: 0 }
    }

    fn slot(table: &mut TableStart, slot: usize, at: usize) {
        table.slots[slot] = at;
        table.top = table.top.max(slot + 1);
    }

    pub(crate) fn u8(&mut self, table: &mut TableStart, slot: usize, value: u8) {
        let at = self.scalar([value]);
        Builder::<A>::slot(table, slot, at);
    }

    pub(crate) fn bool(&mut self, table: &mut TableStart, slot: usize, value: bool) {
        self.u8(table, slot, u8::from(value));
    }

    pub(crate) fn i16(&mut self, table: &mut TableStart, slot: usize, value: i16) {
        let at = self.scalar(value.to_le_bytes());
        Builder::<A>::slot(table, slot, at);
    }

    pub(crate) fn i32(&mut self, table: &mut TableStart, slot: usize, value: i32) {
        let at = self.scalar(value.to_le_bytes());
        Builder::<A>::slot(table, slot, at);
    }

    pub(crate) fn i64(&mut self, table: &mut TableStart, slot: usize, value: i64) {
        let at = self.scalar(value.to_le_bytes());
        Builder::<A>::slot(table, slot, at);
    }

    pub(crate) fn offset(&mut self, table: &mut TableStart, slot: usize, target: usize) {
        let at = self.uoffset(target);
        Builder::<A>::slot(table, slot, at);
    }

    /// The table of the fields added since `table` began, and its vtable before it: its
    /// position.
    pub(crate) fn end(&mut self, table: TableStart) -> usize {
        self.pad(4, 0);
        self.raw(&[0; 4]);
        let at = self.rev.len();
        for slot in (0..table.top).rev() {
            let field = table.slots[slot];
            let offset = if field == 0 { 0 } else { at - field };
            self.raw(&u16::try_from(offset).unwrap_or(u16::MAX).to_le_bytes());
        }
        self.raw(&u16::try_from(at - table.start).unwrap_or(u16::MAX).to_le_bytes());
        self.raw(&u16::try_from(4 + 2 * table.top).unwrap_or(u16::MAX).to_le_bytes());
        // The table's first field is the distance back to its vtable, which lies before it.
        let back = i32::try_from(self.rev.len() - at).unwrap_or(i32::MAX).to_le_bytes();
        for (i, byte) in back.into_iter().enumerate() {
            self.rev[at - 1 - i] = byte;
        }
        at
    }

    /// The buffer, `root` its root table, its length a multiple of 8.
    pub(crate) fn finish(mut self, root: usize, out: &mut Vec<u8, A>) {
        self.pad(8, 4);
        self.uoffset(root);
        out.reserve(self.rev.len());
        for byte in self.rev.iter().rev() {
            out.push(*byte);
        }
    }
}

/// `bytes[at..at + N]`, or why not.
fn take<const N: usize>(bytes: &[u8], at: usize) -> Result<[u8; N], IpcError> {
    let end = at.checked_add(N).ok_or(IpcError::Flatbuffer { what: "an offset past the buffer", at })?;
    let got = bytes.get(at..end).ok_or(IpcError::Flatbuffer { what: "a field past the buffer", at })?;
    let mut out = [0; N];
    out.copy_from_slice(got);
    Ok(out)
}

fn u32_at(bytes: &[u8], at: usize) -> Result<usize, IpcError> {
    Ok(u32::from_le_bytes(take(bytes, at)?).widen_to::<usize>())
}

/// What the offset at `at` points to.
fn follow(bytes: &[u8], at: usize) -> Result<usize, IpcError> {
    at.checked_add(u32_at(bytes, at)?).ok_or(IpcError::Flatbuffer { what: "an offset past the buffer", at })
}

/// A table read in place.
#[derive(Clone, Copy)]
pub(crate) struct Tab<'a> {
    bytes: &'a [u8],
    at: usize,
    vtable: usize,
    vtable_len: usize,
}

impl<'a> Tab<'a> {
    /// The root table of a buffer.
    pub(crate) fn root(bytes: &'a [u8]) -> Result<Tab<'a>, IpcError> {
        Tab::at(bytes, follow(bytes, 0)?)
    }

    fn at(bytes: &'a [u8], at: usize) -> Result<Tab<'a>, IpcError> {
        let back = i64::from(i32::from_le_bytes(take(bytes, at)?));
        let vtable = i64::try_from(at).ok().and_then(|it| it.checked_sub(back)).and_then(|it| usize::try_from(it).ok());
        let vtable = vtable.ok_or(IpcError::Flatbuffer { what: "a vtable before the buffer", at })?;
        let vtable_len = usize::from(u16::from_le_bytes(take(bytes, vtable)?));
        if vtable_len < 4 || vtable.checked_add(vtable_len).is_none_or(|it| it > bytes.len()) {
            return Err(IpcError::Flatbuffer { what: "a vtable past the buffer", at: vtable });
        }
        Ok(Tab { bytes, at, vtable, vtable_len })
    }

    /// Where slot `slot`'s field is, when the table has it.
    fn slot(&self, slot: usize) -> Result<Option<usize>, IpcError> {
        let entry = 4 + 2 * slot;
        if entry + 2 > self.vtable_len {
            return Ok(None);
        }
        let offset = usize::from(u16::from_le_bytes(take(self.bytes, self.vtable + entry)?));
        Ok((offset != 0).then_some(self.at + offset))
    }

    pub(crate) fn u8(&self, slot: usize, default: u8) -> Result<u8, IpcError> {
        Ok(match self.slot(slot)? {
            Some(at) => take::<1>(self.bytes, at)?[0],
            None => default,
        })
    }

    pub(crate) fn bool(&self, slot: usize) -> Result<bool, IpcError> {
        Ok(self.u8(slot, 0)? != 0)
    }

    pub(crate) fn i16(&self, slot: usize, default: i16) -> Result<i16, IpcError> {
        Ok(match self.slot(slot)? {
            Some(at) => i16::from_le_bytes(take(self.bytes, at)?),
            None => default,
        })
    }

    pub(crate) fn i32(&self, slot: usize, default: i32) -> Result<i32, IpcError> {
        Ok(match self.slot(slot)? {
            Some(at) => i32::from_le_bytes(take(self.bytes, at)?),
            None => default,
        })
    }

    pub(crate) fn i64(&self, slot: usize, default: i64) -> Result<i64, IpcError> {
        Ok(match self.slot(slot)? {
            Some(at) => i64::from_le_bytes(take(self.bytes, at)?),
            None => default,
        })
    }

    pub(crate) fn table(&self, slot: usize) -> Result<Option<Tab<'a>>, IpcError> {
        match self.slot(slot)? {
            Some(at) => Ok(Some(Tab::at(self.bytes, follow(self.bytes, at)?)?)),
            None => Ok(None),
        }
    }

    pub(crate) fn string(&self, slot: usize) -> Result<Option<&'a str>, IpcError> {
        let Some((start, len)) = self.vector(slot, 1)? else { return Ok(None) };
        let text = &self.bytes[start..start + len];
        core::str::from_utf8(text).map(Some).map_err(|_| IpcError::Flatbuffer { what: "a string that is not UTF-8", at: start })
    }

    /// Slot `slot`'s vector of elements of `size` bytes: where its elements start and how many.
    fn vector(&self, slot: usize, size: usize) -> Result<Option<(usize, usize)>, IpcError> {
        let Some(at) = self.slot(slot)? else { return Ok(None) };
        let at = follow(self.bytes, at)?;
        let len = u32_at(self.bytes, at)?;
        let start = at + 4;
        let end = len.checked_mul(size).and_then(|it| it.checked_add(start));
        if end.is_none_or(|it| it > self.bytes.len()) {
            return Err(IpcError::Flatbuffer { what: "a vector past the buffer", at });
        }
        Ok(Some((start, len)))
    }

    /// Slot `slot`'s vector of tables: how many, each read by [`Tab::nth`].
    pub(crate) fn tables(&self, slot: usize) -> Result<Tables<'a>, IpcError> {
        let (start, len) = self.vector(slot, 4)?.unwrap_or((0, 0));
        Ok(Tables { bytes: self.bytes, start, len })
    }

    /// Slot `slot`'s vector of structs of `size` bytes, as their bytes.
    pub(crate) fn structs(&self, slot: usize, size: usize) -> Result<&'a [u8], IpcError> {
        Ok(match self.vector(slot, size)? {
            Some((start, len)) => &self.bytes[start..start + len * size],
            None => &[],
        })
    }
}

/// A vector of tables.
#[derive(Clone, Copy)]
pub(crate) struct Tables<'a> {
    bytes: &'a [u8],
    start: usize,
    len: usize,
}

impl<'a> Tables<'a> {
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn nth(&self, i: usize) -> Result<Tab<'a>, IpcError> {
        Tab::at(self.bytes, follow(self.bytes, self.start + 4 * i)?)
    }
}

/// A little-endian `i64` of a struct's bytes at `at`.
pub(crate) fn i64_of(bytes: &[u8], at: usize) -> Result<i64, IpcError> {
    Ok(i64::from_le_bytes(take(bytes, at)?))
}

/// A little-endian `i32` of a struct's bytes at `at`.
pub(crate) fn i32_of(bytes: &[u8], at: usize) -> Result<i32, IpcError> {
    Ok(i32::from_le_bytes(take(bytes, at)?))
}
