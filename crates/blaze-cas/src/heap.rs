//! What a store holds on the global heap, for the daemon's `store heap bytes` counter and for
//! the bound an idle store keeps: a value that has a file or a pack extent is read from there
//! through the pool, never also held here, so a store with nothing pinned holds its tables'
//! slots and no values' bytes.
//!
//! Counted from the capacities of what it owns (the bytes the allocator gave, not the bytes in
//! use), so a table that grew and emptied still counts until it is let go. The tables kept in
//! blaze-mem's regions ([`crate::table::Table`]) are not global heap and are not counted here;
//! they are the footprint's under `Tag::Store`.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::{Store, Tier};

/// The global-heap bytes of every store of this process, as each last counted itself
/// ([`Store::note_heap`]).
static STORE_HEAP: AtomicU64 = AtomicU64::new(0);

/// The global-heap bytes of every store of this process: what `store heap bytes` plots.
pub(crate) fn store_heap() -> u64 {
    STORE_HEAP.load(Ordering::Relaxed)
}

/// The most an idle store (a build ended, nothing pinned, [`Store::trim_pool`] asked) holds
/// on the global heap.
#[cfg(test)]
pub(crate) const IDLE_HEAP_MOST: u64 = 8 << 20;

/// A planted bug for the heap tests (`tests::heap`).
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HeapPlant {
    None,
    /// A commit keeps the capacity its drained bytes had: a pack holds the room of every
    /// record drained into it at once.
    KeepsPackMemory,
    /// A file of its own keeps its bytes on the heap once mapped.
    KeepsExternalMemory,
}

/// Bytes the slots of a table of `capacity` entries of `(K, V)` hold.
fn slots<K, V>(capacity: usize) -> u64 {
    (capacity * (size_of::<(K, V)>() + 1)) as u64
}

fn vec_bytes<T>(it: &Vec<T>) -> u64 {
    (it.capacity() * size_of::<T>()) as u64
}

fn tier_bytes(tier: &Tier) -> u64 {
    let packs: u64 =
        tier.packs.iter().map(|pack| vec_bytes(&pack.memory) + vec_bytes(&pack.checks)).sum();
    let externals: u64 = tier
        .externals
        .values()
        .map(|external| external.memory.as_ref().map_or(0, vec_bytes))
        .sum();
    vec_bytes(&tier.packs)
        + packs
        + slots::<blaze_ids::ContentId, crate::External>(tier.externals.capacity())
        + externals
        + vec_bytes(&tier.removing)
        + vec_bytes(&tier.units)
        + slots::<blaze_ids::ContentId, crate::Unit>(tier.unit_of.capacity())
        + tier.before.as_deref().map_or(0, tier_bytes)
}

impl Store {
    /// What it holds on the global heap now, by the capacities of what it owns: its packs'
    /// drained bytes and checks, its files' bytes not yet mapped, its training samples and its
    /// own tables' slots.
    pub fn heap_bytes(&self) -> u64 {
        let tiers: u64 = self.tiers.iter().map(tier_bytes).sum();
        let training: u64 = self
            .training
            .iter()
            .map(|training| {
                vec_bytes(&training.samples)
                    + training.samples.iter().map(vec_bytes).sum::<u64>()
                    + slots::<blaze_ids::ContentId, ()>(training.sampled.capacity())
            })
            .sum();
        tiers
            + training
            + slots::<crate::Key, crate::read::Seg>(self.copy_homes.capacity())
            + slots::<blaze_core::Tag, tracing::Span>(self.place_spans.capacity())
            + slots::<blaze_ids::ContentId, crate::PlacedFile>(self.files.capacity())
    }

    /// Counts its heap again into the process's `store heap bytes`: once a commit ends and
    /// once a trim ends, the two points it changes by much.
    pub(crate) fn note_heap(&mut self) {
        let now = self.heap_bytes();
        let before = std::mem::replace(&mut self.heap_noted, now);
        if now >= before {
            STORE_HEAP.fetch_add(now - before, Ordering::Relaxed);
        } else {
            STORE_HEAP.fetch_sub(before - now, Ordering::Relaxed);
        }
    }

    /// Takes its count out of the process's as it goes ([`Drop for Store`]).
    pub(crate) fn forget_heap(&mut self) {
        STORE_HEAP.fetch_sub(std::mem::take(&mut self.heap_noted), Ordering::Relaxed);
    }

    /// Lets its own tables give back the slots their removals left, as a trim asks: a table
    /// that grew for one build does not keep that room while the daemon idles.
    pub(crate) fn shrink_tables(&mut self) {
        self.copy_homes.shrink_to_fit();
        self.place_spans.shrink_to_fit();
        self.files.shrink_to_fit();
        for tier in &mut self.tiers {
            tier.externals.shrink_to_fit();
            for pack in &mut tier.packs {
                // The planted bug keeps the drained room through the trim as well, or the
                // trim would undo it and the oracle would see no bug to catch.
                #[cfg(test)]
                let keeps = self.plant_heap == HeapPlant::KeepsPackMemory;
                #[cfg(not(test))]
                let keeps = false;
                if pack.memory.is_empty() && !keeps {
                    // alloc: empty, allocates nothing; it lets go of the pack's drained room
                    pack.memory = Vec::new();
                }
            }
        }
    }
}
