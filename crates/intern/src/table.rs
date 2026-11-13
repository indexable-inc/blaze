//! The table under [`crate::Symbol`]: interned values as byte strings with `u32` IDs, each held
//! for the life of the process.
//!
//! An ID is one of two kinds:
//!
//! - `1..=statics`: compiled in (the declared symbols).
//! - any other below `next`: made by this process, in the overlay: an append-only arena behind
//!   [`Records`]' lock-free index, and a column from ID to record.
//!
//! A lookup probes the overlay's index, placed by SipHash-1-3 of the bytes under a key of the
//! table's own ([`blaze_hash::SipKey`]), never by a fixed hash: the values are names a source
//! file or a macro picks, and a hash anyone can compute lets them pick thousands with one hash,
//! each insert then probing past all the others under the shard's lock (review finding S5:
//! `blaze_phf::name_hash` inverts step by step). The key is drawn at random when the table is
//! made and never leaves the process.
//!
//! Cost: a value takes a 16-byte header beside its bytes and an 8-byte column slot. A lookup is
//! one hash of the bytes and one probe of the index. Reading a value's bytes is a range check
//! and the column's load.
//!
//! Safe code: the overlay's records and ID column are [`crate::typed`]'s.

use blaze_hash::SipKey;

use crate::{
    sync::atomic::{AtomicU32, Ordering},
    typed::{Record, Records},
};

/// What every table of one kind shares.
pub(crate) struct Shape {
    /// IDs `1..=statics` are compiled in.
    pub(crate) statics: u32,
    /// The bytes of compiled-in ID `id`.
    pub(crate) static_bytes: fn(u32) -> &'static [u8],
    /// The order key of a value's bytes.
    pub(crate) key: fn(&[u8]) -> u64,
}

/// The key a table places its values by: drawn from the operating system for each table and
/// never shown, as the values are ones a source file or a macro picks. Under the model checker
/// (`cfg(blaze_check)`) a fixed key instead: the checker runs a model many times and needs each
/// run to read the same, and a placement that moved from run to run would move which shard,
/// group and control word the racing threads touch.
fn overlay_key() -> SipKey {
    match cfg!(blaze_check) {
        true => SipKey::new(0x6f76_6572_6c61_7930, 0x6f76_6572_6c61_7931),
        false => SipKey::random(),
    }
}

/// The alignment of every value's bytes (a [`Records`] header's).
pub(crate) const MAX_ALIGN: usize = 16;

pub(crate) struct Table {
    shape: Shape,
    /// The first ID not compiled in.
    first: u32,
    /// The next ID no value has had.
    next: AtomicU32,
    /// The values this process made, and their ID column.
    records: Records,
    /// What `records` places values by: [`overlay_key`], drawn for each table.
    overlay: SipKey,
}

impl Table {
    /// An empty table, its compiled-in values in the index.
    pub(crate) fn new(shape: Shape) -> Table {
        let first = shape.statics + 1;
        let table = Table {
            first,
            next: AtomicU32::new(first),
            records: Records::new(first),
            overlay: overlay_key(),
            shape,
        };
        for id in 1..first {
            let bytes = (table.shape.static_bytes)(id);
            // A value compiled in twice is the first.
            table
                .records
                .intern(table.overlay.hash(bytes), bytes, || (id, (table.shape.key)(bytes)));
        }
        table
    }

    /// The ID of `bytes`, made if it has none.
    pub(crate) fn intern(&self, bytes: &[u8]) -> u32 {
        let hash = self.overlay.hash(bytes);
        self.records.intern(hash, bytes, || (self.take_id(), (self.shape.key)(bytes))).id()
    }

    /// The ID of `bytes`, if it has one.
    pub(crate) fn find(&self, bytes: &[u8]) -> Option<u32> {
        self.records.find(self.overlay.hash(bytes), bytes).map(Record::id)
    }

    fn take_id(&self) -> u32 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        assert!(id < u32::MAX, "more than 2^32 - 2 values interned in one table");
        id
    }

    /// The bytes of `id`, `None` when it names no value: never made.
    #[inline]
    pub(crate) fn get(&self, id: u32) -> Option<&'static [u8]> {
        if id == 0 {
            return None;
        }
        if id < self.first {
            return Some((self.shape.static_bytes)(id));
        }
        self.records.get(id).map(Record::bytes)
    }

    /// [`Table::get`] of an ID a handle holds, which must name a value.
    #[inline]
    pub(crate) fn bytes(&self, id: u32) -> &'static [u8] {
        self.get(id).unwrap_or_else(|| self.lost(id))
    }

    #[cold]
    #[inline(never)]
    fn lost(&self, id: u32) -> ! {
        panic!(
            "interned ID {id} names no value: it was never made (IDs below {} were)",
            self.next.load(Ordering::Relaxed)
        )
    }

    /// The order key of `id`'s value.
    pub(crate) fn key(&self, id: u32) -> u64 {
        if id < self.first {
            return (self.shape.key)(self.bytes(id));
        }
        self.records.get(id).map_or_else(|| self.lost(id), Record::key)
    }

    /// One past the highest ID made so far.
    #[cfg(test)]
    pub(crate) fn end(&self) -> u32 {
        self.next.load(Ordering::Acquire)
    }
}

/// The control words the tables' probes load on this thread, counted in tests so a test can
/// bound the work of interning values made to collide; nothing in other builds.
pub(crate) mod probes {
    #[cfg(test)]
    thread_local! {
        static LOADED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    #[inline(always)]
    pub(crate) fn count() {
        #[cfg(test)]
        LOADED.with(|it| it.set(it.get() + 1));
    }

    /// The control words loaded since the last call.
    #[cfg(all(test, not(blaze_check)))]
    pub(crate) fn take() -> u64 {
        LOADED.with(|it| it.replace(0))
    }
}

#[cfg(all(test, not(blaze_check)))]
mod tests;

#[cfg(all(test, blaze_check))]
mod model;
