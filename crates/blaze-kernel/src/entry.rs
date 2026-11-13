//! How entries are keyed and laid out in the store. The form is `blaze_record`'s (base key →
//! read-lists → answer), which owns the key derivations, the read-list's layout and the chain;
//! this file is what the kernel puts in it.
//!
//! - **Base key**: the identity's ID ([`Identity::id`]).
//! - **A read's name**: its kind (`u16`, big-endian, so that names order as kind then key),
//!   then its key's bytes ([`named`]).
//! - **Read-list**: `blaze_record::write_list` of the base, no note, and the names of one
//!   run's reads, sorted, each once. It is filed in the base's chain ([`BOUNDS`]).
//! - **Entry key**: `blaze_record::AnswerKey` of the read-list's bytes and each read's digest
//!   (a read that saw nothing is `Digest::Nothing`). With no reads it is [`whole::key`]: the
//!   answer of the list with no reads (`blaze_record::empty_list`), which is known from the
//!   identity and never filed, and that entry's bytes are its bare value.
//! - **Entry bytes** (with reads): magic, step `u16`, `0u16`, the read-list's length `u32`,
//!   the read-list as it is filed, each read's digest in the list's order (`u8` 0 gone / 1 a
//!   hash, then the hash), the hash of the value's bytes (32 bytes), the value's `u64` length,
//!   zeros to a 16-byte boundary, the value. An entry so says by itself which key it is of
//!   ([`Entry::read`]), and what a read of its value sees ([`Entry::value_hash`]) without its
//!   value being hashed again: a parent's lookup reads 32 bytes of each child it names.

use std::ops::Range;

use blaze_cas::{Key, content_of};
use blaze_format::cursor::{Put as _, Reader};
use blaze_ids::ContentId;
use blaze_record::{AnswerKey, Bounds, Digest, ListError, ListRef, empty_list, write_list};

use crate::{Identity, MOST_SHAPES, Read, ReadKind, Seen, Trace};

/// `blaze kernel e`, then the layout's number: 1 kept its reads' names and hashes together
/// under a key of the kernel's own; 2 keeps the read-list `blaze_record` files; 3 keeps the
/// hash of its value's bytes too.
const ENTRY_MAGIC: [u8; 16] = *b"blaze kernel e 3";
const ENTRY_MAGIC_FAMILY: &[u8] = b"blaze kernel e ";

/// The chain of an identity's read-lists: one generation of [`MOST_SHAPES`] slots, searched
/// whole from slot 0 and full at its bound ([`crate::KernelError::TooManyShapes`]), as the
/// kernel's chain of shape slots was. A generational bound (a full chain stops searching its
/// oldest lists and is never a refusal) is a change of what a lookup finds: its own step.
pub(crate) const BOUNDS: Bounds = Bounds::new(MOST_SHAPES, 1);

/// Appends the first part of a read's name: its kind. The key's bytes follow.
pub(crate) fn name_kind(kind: ReadKind, out: &mut Vec<u8>) {
    // alloc: the caller's buffer of one run's names
    out.extend_from_slice(&kind.0.to_be_bytes());
}

/// The read `name` is of; `None` for bytes too short to hold a kind.
pub(crate) fn named(name: &[u8]) -> Option<Read<'_>> {
    let (kind, key) = name.split_first_chunk::<2>()?;
    Some(Read { kind: ReadKind(u16::from_be_bytes(*kind)), key })
}

/// What a read saw, as its entry's key takes it.
pub(crate) fn digest(seen: Seen) -> Digest {
    match seen {
        Seen::Gone => Digest::Nothing,
        Seen::Is(hash) => Digest::Is(hash),
    }
}

/// The key of the entry whose read-list is `list` and whose reads saw `seen`, in the list's
/// order.
pub(crate) fn entry_key(list: &[u8], seen: impl Iterator<Item = Seen>) -> Key {
    let mut key = AnswerKey::new(list);
    for seen in seen {
        key.value(digest(seen));
    }
    Key(key.finish())
}

/// The read-list of `trace` under the identity whose ID is `identity`.
#[cfg(test)]
pub(crate) fn list(identity: &ContentId, trace: &Trace) -> Vec<u8> {
    let mut out = Vec::new();
    write_list(&mut out, identity, &[], trace.names());
    out
}

/// A step whose identity is its whole content: one probe, and the value is kept as it is.
pub mod whole {
    use blaze_cas::Key;

    use crate::Identity;

    /// The key of `identity`'s entry with no reads.
    pub fn key(identity: &Identity) -> Key {
        super::entry_key(&super::empty_list(&identity.id()), std::iter::empty())
    }
}

/// An entry with reads, as the store keeps it.
pub(crate) struct Encoded {
    pub key: Key,
    pub bytes: Vec<u8>,
    /// The hash of its value's bytes, made once here and kept in the entry.
    pub hash: ContentId,
    list: Range<usize>,
}

impl Encoded {
    /// Its read-list, as it is filed in the identity's chain.
    pub fn list(&self) -> &[u8] {
        &self.bytes[self.list.clone()]
    }
}

const LIST_AT: usize = 16 + 2 + 2 + 4;

/// The entry of `identity` whose run made `trace` (which has reads) and `value`.
#[expect(
    blaze::narrowed_len,
    reason = "the entry holds its read-list's length in u32 and nothing caps the names a run reads: a list past 4 GiB needs a typed refusal from encode"
)]
pub(crate) fn encode(identity: &Identity, trace: &Trace, value: &[u8]) -> Encoded {
    let names: usize = trace.names().map(|name| 4 + name.len()).sum();
    let digests: usize = trace.seen().map(|seen| 1 + hash_len(seen)).sum();
    let id = identity.id();
    let list_len = empty_list(&id).len() + names;
    // alloc: the entry, kept in the store: one allocation of its exact size
    let mut out = Vec::with_capacity(LIST_AT + list_len + digests + 32 + 8 + 15 + value.len());
    out.put_bytes(&ENTRY_MAGIC);
    out.put_u16(identity.step.0);
    out.put_u16(0);
    out.put_u32(u32::try_from(list_len).expect("a read-list shorter than 2^32 bytes"));
    write_list(&mut out, &id, &[], trace.names());
    let list = LIST_AT..out.len();
    assert_eq!(list.len(), list_len, "a read-list is its head and each name with its length");
    for seen in trace.seen() {
        match seen {
            Seen::Gone => out.put_u8(0),
            Seen::Is(hash) => {
                out.put_u8(1);
                out.put_bytes(&hash.0);
            }
        }
    }
    let hash = content_of(value);
    out.put_bytes(&hash.0);
    out.put_u64(value.len() as u64);
    out.pad_to(16);
    out.put_bytes(value);
    let key = entry_key(&out[list.clone()], trace.seen());
    Encoded { key, bytes: out, hash, list }
}

fn hash_len(seen: Seen) -> usize {
    match seen {
        Seen::Gone => 0,
        Seen::Is(_) => 32,
    }
}

/// An entry as the store holds it, read in place.
#[derive(Clone, Copy, Debug)]
pub struct Entry<'s> {
    pub key: Key,
    pub identity: Identity,
    pub value: &'s [u8],
    /// The hash of its value's bytes as the entry keeps it; none for an entry with no reads,
    /// whose bytes are its bare value.
    hash: Option<ContentId>,
    /// Its read-list and the digests of the list's reads; none for an entry with no reads.
    reads: Option<(ListRef<'s>, Reader<'s>)>,
}

impl<'s> Entry<'s> {
    /// The entry with no reads of `identity`: its bytes are its value.
    pub fn whole(bytes: &'s [u8], identity: Identity) -> Entry<'s> {
        Entry { key: whole::key(&identity), identity, value: bytes, hash: None, reads: None }
    }

    /// The hash of its value's bytes: what a read of this step's value sees. An entry with
    /// reads keeps it, so nothing is hashed here; an entry with no reads is its bare value
    /// (one probe, no header), which is hashed.
    pub fn value_hash(&self) -> ContentId {
        match self.hash {
            Some(hash) => hash,
            None => content_of(self.value),
        }
    }

    /// The hash the entry keeps of its value, where it keeps one: for a check that the value
    /// under it still hashes to it.
    pub(crate) fn kept_hash(&self) -> Option<ContentId> {
        self.hash
    }

    /// The entry under `key`. It is refused when it is of another identity than `identity`, or
    /// when its reads name another key (filed under a key it does not have).
    pub fn read(bytes: &'s [u8], key: Key, identity: &Identity) -> Result<Entry<'s>, String> {
        if key == whole::key(identity) {
            return Ok(Entry::whole(bytes, *identity));
        }
        let mut input = Reader::new(bytes);
        let magic = input.array::<16>()?;
        if *magic != ENTRY_MAGIC {
            return Err(not_this_layout(magic));
        }
        let step = input.u16()?;
        input.u16()?;
        let list_len = input.u32()? as usize;
        let list_bytes = input.take(list_len)?;
        let id = identity.id();
        let list = match ListRef::open(list_bytes, &id) {
            Ok(list) if step == identity.step.0 => list,
            Ok(_) | Err(ListError::OtherBase) => {
                // alloc: an error
                return Err(format!(
                    "its entry is of step {step} and another identity, not of step {} and identity {id:x}: it is filed under another key",
                    identity.step.0
                ));
            }
            // alloc: an error
            Err(why) => return Err(format!("its entry's reads are {why}")),
        };
        let digests = input;
        let mut keyed = AnswerKey::new(list_bytes);
        for name in list.reads() {
            if named(name).is_none() {
                // alloc: an error
                return Err(format!(
                    "a read named by {} bytes, too few to hold its kind",
                    name.len()
                ));
            }
            keyed.value(digest(next_seen(&mut input)?));
        }
        if Key(keyed.finish()) != key {
            // alloc: an error
            return Err("its reads name another key than the one it is filed under".to_owned());
        }
        let hash = ContentId(*input.array::<32>()?);
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
        Ok(Entry { key, identity: *identity, value, hash: Some(hash), reads: Some((list, digests)) })
    }

    /// The reads it recorded, sorted by kind and key, with the hash each saw.
    pub fn reads(&self) -> EntryReads<'s> {
        match self.reads {
            Some((list, digests)) => EntryReads { names: Some(list.reads()), digests },
            None => EntryReads { names: None, digests: Reader::new(&[]) },
        }
    }
}

/// Why bytes whose magic is `magic` are no entry this blaze reads: an entry of another layout
/// is named as one (kept by another blaze under a key this one never looks under), never read
/// as this layout.
fn not_this_layout(magic: &[u8; 16]) -> String {
    let (family, number) = magic.split_at(ENTRY_MAGIC_FAMILY.len());
    match family == ENTRY_MAGIC_FAMILY && number[0].is_ascii_digit() {
        // alloc: an error
        true => format!(
            "its value is a kernel entry of layout {}, where this blaze reads layout {}",
            number[0] as char,
            ENTRY_MAGIC[ENTRY_MAGIC_FAMILY.len()] as char
        ),
        // alloc: an error
        false => "its value is no kernel entry".to_owned(),
    }
}

fn next_seen(input: &mut Reader<'_>) -> Result<Seen, String> {
    match input.u8()? {
        0 => Ok(Seen::Gone),
        1 => Ok(Seen::Is(ContentId(*input.array::<32>()?))),
        // alloc: an error
        tag => Err(format!("a read seen as {tag}, which is neither 0 (gone) nor 1 (a hash)")),
    }
}

/// An entry's reads, checked when the entry was read ([`Entry::read`]).
#[derive(Clone, Debug)]
pub struct EntryReads<'s> {
    names: Option<blaze_record::Reads<'s>>,
    digests: Reader<'s>,
}

impl<'s> Iterator for EntryReads<'s> {
    type Item = (Read<'s>, Seen);

    fn next(&mut self) -> Option<(Read<'s>, Seen)> {
        let checked = "an entry's reads, checked by `Entry::read`";
        let name = self.names.as_mut()?.next()?;
        Some((named(name).expect(checked), next_seen(&mut self.digests).expect(checked)))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.names.as_ref().map_or(0, ExactSizeIterator::len);
        (left, Some(left))
    }
}

impl ExactSizeIterator for EntryReads<'_> {}
