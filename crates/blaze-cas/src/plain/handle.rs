//! The plain store as blaze-keyed's [`Store`]: the handle one process holds.
//!
//! Reads take no lock. Every segment this process mapped (the generation it opened on, then
//! what each of its commits and compactions added) is in an insert-only table, each under the
//! index it was mapped at and never removed or replaced: a read probes the segments of the
//! namespaces it asks, in their order, each namespace's newest first, and returns the bytes in
//! place (`map`) or copied into the caller's frame (`read`), with the segment's [`Proof`]. No
//! read checks a byte (decision 18): each segment was proved once, when it was mapped.
//!
//! Writes go through the one writer, [`Driven`], under a lock: a put, the commit (the segment
//! writes, then the flip under the store's one file lock), a compaction. The one place a read
//! takes that lock: a miss while this process holds records put and not yet committed (they
//! are in the writer's batches, not in a mapped segment); a read that hits a mapped segment,
//! or misses with nothing held, never does.

use std::{
    alloc::{Allocator, Global},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use blaze_core::{Driver, Mapping, Native, lock::Lock};
use blaze_hash::SharedMap;
use blaze_keyed::{
    kind::{Key as KeyOf, Kind, Namespace},
    store::{Compacted, ENTRY, Generation, Put, Read, RootRef, Store, StoreError},
};

use super::{
    core::PutError,
    driven::Driven,
    format::{self, Entry, Segment},
    settings::Settings,
    verified::Proof,
};
use crate::Key;

/// One segment this process mapped: its row, its mapping (a clone, kept for the life of the
/// handle: never unmapped while the process runs), and its proof.
struct Mapped {
    seg: Segment,
    map: Mapping,
    proof: Proof,
}

impl Mapped {
    fn run(&self) -> &[u8] {
        self.seg.run(self.map.bytes())
    }

    fn payload(&self, entry: &Entry) -> &[u8] {
        &self.map.bytes()[entry.at as usize..(entry.at + u64::from(entry.len)) as usize]
    }
}

/// The plain store, as one process holds it: lock-free reads, one writer.
pub struct PlainStore<D: Driver = Native> {
    writer: Lock<Driven<D>>,
    /// Index `i` is the `i`th segment mapped; written under the writer's lock, read without it.
    table: SharedMap<usize, Mapped>,
    /// How many of `table`'s indexes are in: published with `Release` after their inserts.
    len: AtomicUsize,
    /// The namespaces a read asks, in order.
    reads: Vec<Namespace>,
    /// This process holds records put and not yet committed.
    uncommitted: AtomicBool,
}

impl PlainStore<Native> {
    /// The plain store in `root/plain`, with the namespaces `settings` name.
    pub fn open(root: &std::path::Path, settings: &Settings) -> Result<PlainStore<Native>, StoreError> {
        Driven::open(root, settings).map(PlainStore::new)
    }
}

impl<D: Driver> PlainStore<D> {
    /// The handle over an opened store.
    pub fn new(driven: Driven<D>) -> PlainStore<D> {
        let mut reads = Vec::with_capacity_in(driven.core().reads().len(), Global);
        reads.extend_from_slice(driven.core().reads());
        let it = PlainStore { writer: Lock::new("store.plain.writer", driven), table: SharedMap::default(), len: AtomicUsize::new(0), reads, uncommitted: AtomicBool::new(false) };
        it.refresh(&it.writer.lock());
        it
    }

    /// What the writer mapped since the last refresh, into the table; called under the
    /// writer's lock, so one refresher at a time.
    fn refresh(&self, writer: &Driven<D>) {
        let core = writer.core();
        let generation = Generation(core.generation());
        let from = self.len.load(Ordering::Relaxed);
        let table = self.table.pin();
        for at in from..core.mapped_count() {
            let proved = core.mapped_at(at);
            let replaced = table.insert(at, Mapped { seg: *proved.seg(), map: proved.mapping().clone(), proof: proved.proof(generation) });
            assert!(replaced.is_none(), "the table is insert-only: index {at} is inserted once");
        }
        self.len.store(core.mapped_count(), Ordering::Release);
        self.uncommitted.store(core.holds_uncommitted(), Ordering::Release);
    }

    /// The segment and entry of `key` in `self.reads`, newest first within each namespace; no
    /// lock, no check.
    fn find(&self, key: &blaze_ids::ContentId) -> Option<(&Mapped, Entry)> {
        let len = self.len.load(Ordering::Acquire);
        let table = self.table.pin();
        for space in &self.reads {
            for at in (0..len).rev() {
                let Some(mapped) = table.get(&at) else { continue };
                if mapped.seg.namespace != *space {
                    continue;
                }
                let run = mapped.run();
                if let Some(i) = format::find(run, key) {
                    // SAFETY: the table is insert-only: each index is inserted once, under the
                    // writer's lock, and never removed or replaced, so papaya never retires the
                    // allocation of a value while the map lives; values are allocated one by
                    // one and a resize moves pointers, not values. The map lives as long as
                    // `self`, so the value outlives the guard's borrow for `&self`'s lifetime.
                    // Its mapping's bytes are the segment's for as long as the clone lives.
                    let mapped: &Mapped = unsafe { &*(mapped as *const Mapped) };
                    return Some((mapped, Entry::read(&run[i * ENTRY..(i + 1) * ENTRY])));
                }
            }
        }
        None
    }

    /// The writer, for what the trait does not cover (pull, publish, scrub).
    pub fn writer(&self) -> blaze_core::lock::LockGuard<'_, Driven<D>> {
        self.writer.lock()
    }
}

/// A record put and not yet committed has no segment: its proof is this process's own segment
/// to be, which a segment this process wrote is trusted as (decision 18).
fn own_proof(generation: u64) -> Proof {
    Proof::own(Generation(generation))
}

impl<D: Driver> Store for PlainStore<D> {
    type Held<A: Allocator> = Vec<u8, A>;
    type Verified = Proof;

    /// Copied from its mapped segment into `frame` (the segment is mapped already, so this is
    /// the page cache's bytes with no system call; `Driven::get_into` is the `pread` path for a
    /// caller with no mapping).
    fn read<K: Kind, A: Allocator>(&self, key: &KeyOf<K>, frame: A) -> Result<Option<Read<Proof, Vec<u8, A>>>, StoreError> {
        if let Some((mapped, entry)) = self.find(&key.id()) {
            if entry.tag != K::TAG {
                return Err(StoreError::TwoValues { tag: K::TAG, key: key.id() });
            }
            let payload = mapped.payload(&entry);
            let mut bytes = Vec::with_capacity_in(payload.len(), frame);
            bytes.extend_from_slice(payload);
            return Ok(Some(Read { bytes, from: mapped.proof }));
        }
        if !self.uncommitted.load(Ordering::Acquire) {
            return Ok(None);
        }
        // The one lock a read takes: records put and not yet committed are the writer's.
        let writer = self.writer.lock();
        let Some(got) = writer.core().get(&Key(key.id())) else { return Ok(None) };
        if got.tag != K::TAG {
            return Err(StoreError::TwoValues { tag: K::TAG, key: key.id() });
        }
        let mut bytes = Vec::with_capacity_in(got.bytes.len(), frame);
        bytes.extend_from_slice(got.bytes);
        Ok(Some(Read { bytes, from: own_proof(writer.core().generation()) }))
    }

    /// In place in its mapped segment. A record put and not yet committed is in no segment, so
    /// it is not mapped: [`StoreError::Uncommitted`] names it, never a quiet `None`.
    fn map<K: Kind>(&self, key: &KeyOf<K>) -> Result<Option<Read<Proof, &[u8]>>, StoreError> {
        if let Some((mapped, entry)) = self.find(&key.id()) {
            if entry.tag != K::TAG {
                return Err(StoreError::TwoValues { tag: K::TAG, key: key.id() });
            }
            return Ok(Some(Read { bytes: mapped.payload(&entry), from: mapped.proof }));
        }
        if self.uncommitted.load(Ordering::Acquire) && self.writer.lock().core().get(&Key(key.id())).is_some() {
            return Err(StoreError::Uncommitted { tag: K::TAG, key: key.id() });
        }
        Ok(None)
    }

    fn put<K: Kind>(&self, key: &KeyOf<K>, namespace: Namespace, bytes: &[u8]) -> Result<Put, StoreError> {
        let mut writer = self.writer.lock();
        match writer.put(Key(key.id()), namespace, K::TAG, bytes) {
            Ok(put) => {
                if put == Put::Written {
                    self.uncommitted.store(true, Ordering::Release);
                }
                Ok(put)
            }
            // Other bytes under the key: an answer, not a failure.
            Err(PutError::Mismatch(_)) => Ok(Put::Other),
            Err(refused) => Err(refused.into_store(K::TAG)),
        }
    }

    fn commit(&self) -> Result<Generation, StoreError> {
        let mut writer = self.writer.lock();
        let committed = writer.commit_with(Vec::new_in(Global))?;
        self.refresh(&writer);
        Ok(Generation(committed.generation))
    }

    /// Roots `roots`' records under the root `compact` (each record kept, with its key: the
    /// store cannot read what a record of another kind names, so what a root's record names is
    /// the caller's to root too), sets the cap, commits, then compacts in bounded slices on the
    /// calling thread as it is (the caller runs it beside builds, on a thread it lowered).
    fn compact(&self, roots: &[RootRef], cap: u64) -> Result<Compacted, StoreError> {
        let mut writer = self.writer.lock();
        let mut keys = Vec::with_capacity_in(roots.len(), Global);
        keys.extend(roots.iter().map(|it| Key(it.key)));
        writer.root("compact", &keys)?;
        writer.core_mut().set_cap(cap);
        writer.commit_with(Vec::new_in(Global))?;
        // Each slice replaces at least one segment, so one a segment (and one to say there is
        // no more) bounds them.
        let most = writer.core().segments().len() as u32 + 1;
        let slices = writer.slices(most)?;
        self.refresh(&writer);
        let mut out = Compacted::default();
        for it in &slices {
            out.kept += it.kept;
            out.freed += it.dropped;
            out.kept_bytes += it.bytes_out;
            out.freed_bytes += it.bytes_in.saturating_sub(it.bytes_out);
        }
        Ok(out)
    }
}
