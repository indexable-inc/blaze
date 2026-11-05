//! How entries and shapes are keyed and laid out in the store.
//!
//! - **Entry key**: `H("blaze v2 kernel entry 1", identity, count, each (kind, key, seen))`.
//!   With no reads it is [`whole::key`], and that entry's bytes are its bare value.
//! - **Entry bytes** (with reads): magic, step `u16`, `0u16`, count `u32`, the identity's
//!   ID, then each read (kind `u16`, seen `u8` 0 gone / 1 hash, the hash, key `u32` length and
//!   bytes), then the value's `u64` length, zeros to a 16-byte boundary, and the value.
//! - **Shape slot `n`**: `H("blaze v2 kernel shape slot 1", identity, n)`. Its bytes are
//!   magic, the identity's ID, count `u32`, then each read's kind `u16` and key (`u32` length
//!   and bytes), with no hashes.

use blaze_cas::Key;
use blaze_format::cursor::{Put as _, Reader};
use blaze_ids::{ContentId, Hasher};

use crate::{Identity, Read, ReadKind, Seen, Trace};

const ENTRY_KEY: &str = "blaze v2 kernel entry 1";
const SLOT_KEY: &str = "blaze v2 kernel shape slot 1";
const ENTRY_MAGIC: [u8; 16] = *b"blaze kernel e 1";
const SHAPE_MAGIC: [u8; 16] = *b"blaze kernel s 1";

/// An entry's key, made read by read.
pub struct EntryKey(Hasher);

impl EntryKey {
    pub fn new(identity: &ContentId, reads: usize) -> EntryKey {
        let mut hasher = Hasher::new(ENTRY_KEY);
        hasher.id(identity).update(&(reads as u64).to_le_bytes());
        EntryKey(hasher)
    }

    pub fn read(&mut self, read: Read<'_>, seen: Seen) {
        self.0.update(&read.kind.0.to_le_bytes()).part(read.key);
        match seen {
            Seen::Gone => self.0.update(&[0]),
            Seen::Is(hash) => self.0.update(&[1]).id(&hash),
        };
    }

    pub fn finish(&self) -> Key {
        Key(self.0.finish())
    }
}

/// The key of the entry of `identity` (its [`Identity::id`]) whose reads saw `reads`.
pub fn entry_key<'a>(
    identity: &ContentId,
    reads: impl ExactSizeIterator<Item = (Read<'a>, Seen)>,
) -> Key {
    let mut key = EntryKey::new(identity, reads.len());
    for (read, seen) in reads {
        key.read(read, seen);
    }
    key.finish()
}

/// The key of slot `n` of `identity`'s chain of shapes.
pub fn shape_slot(identity: &ContentId, n: u32) -> Key {
    let mut hasher = Hasher::new(SLOT_KEY);
    hasher.id(identity).update(&n.to_le_bytes());
    Key(hasher.finish())
}

/// A step whose identity is its whole content: one probe, and the value is kept as it is.
pub mod whole {
    use blaze_cas::Key;

    use crate::Identity;

    /// The key of `identity`'s entry with no reads.
    pub fn key(identity: &Identity) -> Key {
        super::entry_key(&identity.id(), std::iter::empty())
    }
}

/// An entry with reads, as the store keeps it.
pub(crate) fn encode(identity: &Identity, trace: &Trace, value: &[u8]) -> Vec<u8> {
    if trace.is_empty() {
        // alloc: the value, kept in the store
        return value.to_vec();
    }
    let reads: usize =
        trace.iter().map(|(read, seen)| 2 + 1 + hash_len(seen) + 4 + read.key.len()).sum();
    // alloc: the entry, kept in the store: one allocation of its exact size
    let mut out = Vec::with_capacity(16 + 8 + 32 + reads + 8 + 15 + value.len());
    out.put_bytes(&ENTRY_MAGIC);
    out.put_u16(identity.step.0);
    out.put_u16(0);
    out.put_u32(u32::try_from(trace.len()).expect("fewer than 2^32 reads in one run"));
    out.put_bytes(&identity.id().0);
    for (read, seen) in trace.iter() {
        out.put_u16(read.kind.0);
        match seen {
            Seen::Gone => out.put_u8(0),
            Seen::Is(hash) => {
                out.put_u8(1);
                out.put_bytes(&hash.0);
            }
        }
        out.put_bytes_u32(read.key);
    }
    out.put_u64(value.len() as u64);
    out.pad_to(16);
    out.put_bytes(value);
    out
}

fn hash_len(seen: Seen) -> usize {
    match seen {
        Seen::Gone => 0,
        Seen::Is(_) => 32,
    }
}

/// The shape of `trace`: its reads' names, without their hashes.
pub(crate) fn encode_shape(identity: &ContentId, trace: &Trace) -> Vec<u8> {
    let reads: usize = trace.iter().map(|(read, _)| 2 + 4 + read.key.len()).sum();
    // alloc: the shape, kept in the store: one allocation of its exact size
    let mut out = Vec::with_capacity(16 + 32 + 4 + reads);
    out.put_bytes(&SHAPE_MAGIC);
    out.put_bytes(&identity.0);
    out.put_u32(u32::try_from(trace.len()).expect("fewer than 2^32 reads in one run"));
    for (read, _) in trace.iter() {
        out.put_u16(read.kind.0);
        out.put_bytes_u32(read.key);
    }
    out
}

/// An entry as the store holds it, read in place.
#[derive(Clone, Copy, Debug)]
pub struct Entry<'s> {
    pub key: Key,
    pub identity: Identity,
    pub value: &'s [u8],
    reads: Reader<'s>,
    count: usize,
}

impl<'s> Entry<'s> {
    /// The entry with no reads of `identity`: its bytes are its value.
    pub fn whole(bytes: &'s [u8], identity: Identity) -> Entry<'s> {
        Entry {
            key: whole::key(&identity),
            identity,
            value: bytes,
            reads: Reader::new(&[]),
            count: 0,
        }
    }

    /// The entry under `key`. It is refused when it is of another identity than `identity`, or
    /// when its reads name another key (filed under a key it does not have).
    pub fn read(bytes: &'s [u8], key: Key, identity: &Identity) -> Result<Entry<'s>, String> {
        if key == whole::key(identity) {
            return Ok(Entry::whole(bytes, *identity));
        }
        let mut input = Reader::new(bytes);
        if *input.array::<16>()? != ENTRY_MAGIC {
            // alloc: an error
            return Err("its value is no kernel entry".to_owned());
        }
        let step = input.u16()?;
        input.u16()?;
        let count = input.u32()? as usize;
        let id = ContentId(*input.array::<32>()?);
        if step != identity.step.0 || id != identity.id() {
            // alloc: an error
            return Err(format!(
                "its entry is of step {step} and identity {id:x}, not of step {} and identity {:x}: it is filed under another key",
                identity.step.0,
                identity.id()
            ));
        }
        let reads = input;
        let mut keyed = EntryKey::new(&id, count);
        for _ in 0..count {
            let (read, seen) = next_read(&mut input)?;
            keyed.read(read, seen);
        }
        if keyed.finish() != key {
            // alloc: an error
            return Err("its reads name another key than the one it is filed under".to_owned());
        }
        // alloc: an error
        let len =
            usize::try_from(input.u64()?).map_err(|_| "a value longer than memory".to_owned())?;
        let pad = input.at().next_multiple_of(16) - input.at();
        if input.take(pad)?.iter().any(|it| *it != 0) {
            // alloc: an error
            return Err("its value's padding is not zero".to_owned());
        }
        let value = input.take(len)?;
        input.finish()?;
        Ok(Entry { key, identity: *identity, value, reads, count })
    }

    /// The reads it recorded, sorted by kind and key, with the hash each saw.
    pub fn reads(&self) -> EntryReads<'s> {
        EntryReads { input: self.reads, left: self.count }
    }
}

fn next_read<'s>(input: &mut Reader<'s>) -> Result<(Read<'s>, Seen), String> {
    let kind = ReadKind(input.u16()?);
    let seen = match input.u8()? {
        0 => Seen::Gone,
        1 => Seen::Is(ContentId(*input.array::<32>()?)),
        // alloc: an error
        tag => {
            return Err(format!("a read seen as {tag}, which is neither 0 (gone) nor 1 (a hash)"));
        }
    };
    let key = input.bytes_u32()?;
    Ok((Read { kind, key }, seen))
}

/// An entry's reads, checked when the entry was read ([`Entry::read`]).
#[derive(Clone, Copy, Debug)]
pub struct EntryReads<'s> {
    input: Reader<'s>,
    left: usize,
}

impl<'s> Iterator for EntryReads<'s> {
    type Item = (Read<'s>, Seen);

    fn next(&mut self) -> Option<(Read<'s>, Seen)> {
        self.left = self.left.checked_sub(1)?;
        Some(next_read(&mut self.input).expect("an entry's reads, checked by `Entry::read`"))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

impl ExactSizeIterator for EntryReads<'_> {}

/// The names of the reads some entry of an identity recorded, kept in a slot of its chain.
#[derive(Clone, Copy, Debug)]
pub struct Shape<'s> {
    reads: Reader<'s>,
    count: usize,
}

impl<'s> Shape<'s> {
    /// The shape of the identity whose ID is `identity`, checked whole.
    pub fn read(bytes: &'s [u8], identity: &ContentId) -> Result<Shape<'s>, String> {
        let mut input = Reader::new(bytes);
        if *input.array::<16>()? != SHAPE_MAGIC {
            // alloc: an error
            return Err("its value is no kernel shape".to_owned());
        }
        let id = ContentId(*input.array::<32>()?);
        if id != *identity {
            // alloc: an error
            return Err(format!(
                "its shape is of identity {id:x}, not {identity:x}: it is filed under another key"
            ));
        }
        let count = input.u32()? as usize;
        let reads = input;
        for _ in 0..count {
            next_name(&mut input)?;
        }
        input.finish()?;
        Ok(Shape { reads, count })
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn reads(&self) -> ShapeReads<'s> {
        ShapeReads { input: self.reads, left: self.count }
    }
}

fn next_name<'s>(input: &mut Reader<'s>) -> Result<Read<'s>, String> {
    let kind = ReadKind(input.u16()?);
    let key = input.bytes_u32()?;
    Ok(Read { kind, key })
}

/// A shape's reads, checked when the shape was read ([`Shape::read`]).
#[derive(Clone, Copy, Debug)]
pub struct ShapeReads<'s> {
    input: Reader<'s>,
    left: usize,
}

impl<'s> Iterator for ShapeReads<'s> {
    type Item = Read<'s>;

    fn next(&mut self) -> Option<Read<'s>> {
        self.left = self.left.checked_sub(1)?;
        Some(next_name(&mut self.input).expect("a shape's reads, checked by `Shape::read`"))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

impl ExactSizeIterator for ShapeReads<'_> {}
