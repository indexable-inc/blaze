//! The production `Resident` record memo of blaze-keyed's driver (`blaze_keyed::memo::Memo`):
//! records a helper process keeps between commands, a table per crate arena in that arena.
//!
//! What it keeps: records of the kinds whose `KEEP` is `DiskAndResident`, by (tag, key), each a
//! 16-aligned copy in its arena. Its bound: `cap_bytes` of records; a keep past it drops every
//! arena wholesale, as `blaze_keyed::memo::Resident` does, and they refill from the store. It
//! is a cache of the store: every record it answers is one the store holds under the same key,
//! so a process holding it can be killed at any moment and loses nothing.
//!
//! The refinement: answer for answer equal to `blaze_keyed::memo::Resident` (and so to
//! `RecordsOnly`, by blaze-keyed's own test) for any sequence of gets, keeps, drops and request
//! ends (`tests/resident.rs`).

use std::{alloc::Global, alloc::Layout};

use blaze_ids::ContentId;
use blaze_keyed::{
    kind::{Keep, Key, Kind as RecordKind},
    memo::{Arena, Memo},
    proof::Holds,
};
use blaze_mem::Tag;

use crate::{
    ArenaId, CycleKind, Kind,
    arena::{Slot, with_local},
    fetch::hash_of,
    trie::{Entry, Head},
};

static RECORD: Kind = Kind::new("blaze_memo::resident::record", CycleKind::Panic);

/// A record's bytes in its arena.
#[derive(Clone, Copy)]
struct Bytes {
    at: *const u8,
    len: usize,
}

type RecordEntry = Entry<(u16, ContentId), Bytes>;

/// A table of records per arena, bounded by bytes.
pub struct Resident {
    arenas: Vec<Option<(Box<Slot, Global>, u64)>, Global>,
    cap_bytes: u64,
    bytes: u64,
}

impl Resident {
    pub fn new(cap_bytes: u64) -> Resident {
        Resident { arenas: Vec::new_in(Global), cap_bytes, bytes: 0 }
    }

    fn find(&self, arena: Arena, tag: u16, id: ContentId) -> Option<&RecordEntry> {
        let (slot, _) = self.arenas.get(arena as usize)?.as_ref()?;
        let key = (tag, id);
        let head = slot.trie.get(hash_of(&RECORD, &key), |head: &Head| {
            // SAFETY: every entry of this table is a `RecordEntry`.
            unsafe { RecordEntry::of(head) }.key == key
        })?;
        // SAFETY: as above.
        Some(unsafe { RecordEntry::of(head) })
    }
}

impl Memo for Resident {
    const NAME: &'static str = "resident (blaze-memo)";

    fn get<K: RecordKind>(&self, arena: Arena, key: &Key<K>) -> Option<&[u8]> {
        let entry = self.find(arena, K::TAG.0, key.id())?;
        // SAFETY: `len` bytes kept in the arena, which `self` holds.
        Some(unsafe { std::slice::from_raw_parts(entry.value.at, entry.value.len) })
    }

    fn keep<K: RecordKind>(&mut self, arena: Arena, holds: &Holds<'_, K>, record: &[u8]) {
        // The proof is the check: nothing kept here was unproved in the asking request, and
        // nothing here checks it again (decision 18).
        let key = &holds.key();
        if K::KEEP == Keep::Disk {
            return;
        }
        let len = record.len() as u64;
        if self.bytes + len > self.cap_bytes {
            // Over the bound: every arena goes, wholesale; they refill from the store.
            self.arenas.clear();
            self.bytes = 0;
            if len > self.cap_bytes {
                return;
            }
        }
        if self.find(arena, K::TAG.0, key.id()).is_some() {
            return;
        }
        assert!(arena < crate::MAX_ARENAS, "arena {arena} is past {}", crate::MAX_ARENAS);
        let at = arena as usize;
        if self.arenas.len() <= at {
            self.arenas.resize_with(at + 1, || None);
        }
        let (slot, held) = self.arenas[at].get_or_insert_with(|| {
            let id = ArenaId::new(arena, 0);
            (Box::new_in(Slot::new(id, Tag::Store, 0, 1), Global), 0)
        });
        let slot: &Slot = slot;
        let key = (K::TAG.0, key.id());
        let hash = hash_of(&RECORD, &key);
        let entry = with_local(slot, |local| -> *const RecordEntry {
            let layout = Layout::from_size_align(record.len().max(1), 16).expect("a record's size");
            let bytes = local.alloc_layout(layout).as_ptr();
            // SAFETY: `bytes` is `record.len()` fresh bytes at least.
            unsafe { std::ptr::copy_nonoverlapping(record.as_ptr(), bytes, record.len()) };
            local.alloc(Entry {
                head: Head::new(hash, &RECORD, &[], 0),
                key,
                value: Bytes { at: bytes, len: record.len() },
            })
        });
        // SAFETY: just made in the slot's arena.
        let head = unsafe { &(*entry).head };
        let fresh = with_local(slot, |local| {
            slot.trie
                .insert(local, head, |other: &Head| {
                    // SAFETY: as in `find`.
                    unsafe { RecordEntry::of(other) }.key == key
                })
                .is_ok()
        });
        debug_assert!(fresh, "looked up absent above, and `&mut self` excludes a racer");
        *held += len;
        self.bytes += len;
    }

    fn drop_arena(&mut self, arena: Arena) {
        if let Some(it) = self.arenas.get_mut(arena as usize)
            && let Some((_, held)) = it.take()
        {
            self.bytes -= held;
        }
    }

    fn end_request(&mut self) {}

    fn resident_bytes(&self) -> u64 {
        self.bytes
    }
}
