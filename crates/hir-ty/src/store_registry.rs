//! The one table of stores installed per database, which the item store (`body_store`) and the
//! goal store (`next_solver::goal_store`) both keep theirs in: a slot per database with a store
//! at once, found by a lock-free walk on every lookup and offer, and grown a segment of
//! [`SEGMENT`] slots at a time, so no count of databases is a limit a caller meets (a test
//! binary runs its tests' databases side by side). A segment, once linked, is the process's for
//! good, so a slot's address stays valid for the guard that holds it; the table is as long as
//! the most databases that had a store at one time. An unlinked store is freed through the
//! registry's limbo once every reader that could have loaded it has left its reclaim task.

use std::{
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
};

use blaze_core::lock::Lock;
use blaze_reclaim::Limbo;

/// How many stores one [`Segment`] holds.
const SEGMENT: usize = 16;

/// A run of a registry's slots, and the run after it, made once this one filled.
struct Segment<T> {
    slots: [AtomicPtr<T>; SEGMENT],
    next: AtomicPtr<Segment<T>>,
}

impl<T> Segment<T> {
    const fn new() -> Segment<T> {
        Segment {
            slots: [const { AtomicPtr::new(ptr::null_mut()) }; SEGMENT],
            next: AtomicPtr::new(ptr::null_mut()),
        }
    }

    fn next(&self) -> Option<&Segment<T>> {
        // SAFETY: a segment linked here is a leaked `Box` (`Registry::link`), never freed.
        unsafe { self.next.load(Ordering::Acquire).as_ref() }
    }
}

/// The installed stores of one kind, each a `Box` given to [`Registry::link`], freed through
/// the registry's limbo once its [`Linked`] guard drops. Kept in a `static`.
pub(crate) struct Registry<T> {
    /// How many are linked: zero is the one load every entry point makes.
    linked: AtomicUsize,
    head: Segment<T>,
    /// The one reclamation pool of this registry: every store unlinked from it is retired here.
    limbo: Lock<Limbo>,
}

impl<T: Send + Sync + 'static> Registry<T> {
    /// An empty registry; `limbo` names its limbo's lock.
    pub(crate) const fn new(limbo: &'static str) -> Registry<T> {
        Registry {
            linked: AtomicUsize::new(0),
            head: Segment::new(),
            limbo: Lock::new(limbo, Limbo::new()),
        }
    }

    /// Whether no store is linked: one relaxed load, for entry points to return early on.
    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.linked.load(Ordering::Relaxed) == 0
    }

    /// Every slot, in order.
    fn slots(&self) -> impl Iterator<Item = &AtomicPtr<T>> {
        std::iter::successors(Some(&self.head), |it| it.next()).flat_map(|it| it.slots.iter())
    }

    /// Runs `f` on the first linked store `pick` takes, if there is one.
    #[inline]
    pub(crate) fn with<R>(&self, pick: impl Fn(&T) -> bool, f: impl FnOnce(&T) -> R) -> Option<R> {
        if self.is_empty() {
            return None;
        }
        let _task = blaze_reclaim::enter();
        for slot in self.slots() {
            let held = slot.load(Ordering::Acquire);
            // SAFETY: a store is freed only through the limbo, after a grace started once its
            // `Linked` drop unlinked it; `_task` was entered before `held` was loaded and outlives
            // `f`, so that grace has not passed while the store is used here.
            let Some(store) = (unsafe { held.as_ref() }) else { continue };
            if pick(store) {
                return Some(f(store));
            }
        }
        None
    }

    /// Links `store` into the first free slot, growing the table by a segment when none is
    /// free, and gives the guard that unlinks it. Bounded: each round tries one segment's slots
    /// and moves to the next, and a segment is made only past the last one, so the rounds are at
    /// most the segments a free slot is found in or before, which the stores linked at once
    /// bound.
    pub(crate) fn link(&'static self, store: Box<T>) -> Linked<T> {
        let store = Box::into_raw(store);
        let mut segment: &'static Segment<T> = &self.head;
        loop {
            for slot in &segment.slots {
                if slot
                    .compare_exchange(ptr::null_mut(), store, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    self.linked.fetch_add(1, Ordering::Relaxed);
                    // SAFETY: `Box::into_raw` never gives null.
                    let store = unsafe { NonNull::new_unchecked(store) };
                    return Linked { registry: self, slot, store };
                }
            }
            segment = match segment.next() {
                Some(next) => next,
                None => {
                    // alloc: a segment of the registry, once per 16 databases with a store at
                    // one time, kept for the process (see the module's docs)
                    let grown = Box::into_raw(Box::new(Segment::new()));
                    match segment.next.compare_exchange(
                        ptr::null_mut(),
                        grown,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        // SAFETY: just leaked, and linked: never freed.
                        Ok(_) => unsafe { &*grown },
                        Err(won) => {
                            // SAFETY: never linked, so this is its only owner.
                            drop(unsafe { Box::from_raw(grown) });
                            // SAFETY: as `Segment::next`: a linked segment is never freed.
                            unsafe { &*won }
                        }
                    }
                }
            };
        }
    }
}

/// A store linked into a [`Registry`]; dropping it unlinks the store and retires it.
pub(crate) struct Linked<T: Send + Sync + 'static> {
    registry: &'static Registry<T>,
    slot: &'static AtomicPtr<T>,
    store: NonNull<T>,
}

// SAFETY: `Linked` owns its store, which is `Send + Sync`, and hands out only shared references
// to it.
unsafe impl<T: Send + Sync + 'static> Send for Linked<T> {}
// SAFETY: as above.
unsafe impl<T: Send + Sync + 'static> Sync for Linked<T> {}

impl<T: Send + Sync + 'static> Linked<T> {
    /// The store.
    pub(crate) fn get(&self) -> &T {
        // SAFETY: the store is freed only by `drop`, which takes `&mut self`.
        unsafe { self.store.as_ref() }
    }
}

impl<T: Send + Sync + 'static> Drop for Linked<T> {
    fn drop(&mut self) {
        let held = self.slot.swap(ptr::null_mut(), Ordering::AcqRel);
        assert_eq!(
            held,
            self.store.as_ptr(),
            "the slot of a store in registry `{}` held another store",
            self.registry.limbo.name()
        );
        self.registry.linked.fetch_sub(1, Ordering::Relaxed);
        // SAFETY: made by `Box::into_raw` in `link`, and unlinked by the swap above, which only
        // this guard makes; readers that loaded it are covered by the limbo's grace.
        let store = unsafe { Box::from_raw(held) };
        let mut limbo = self.registry.limbo.lock();
        limbo.retire(store);
        // alloc: `Limbo::collect` frees; it is not an iterator's.
        limbo.collect();
    }
}

#[cfg(test)]
mod tests {
    use super::{Linked, Registry, SEGMENT};

    /// What a registry finds for each of `ids`: the id of the store `with` picks for it.
    fn found(registry: &Registry<u64>, ids: &[u64]) -> Vec<Option<u64>> {
        ids.iter().map(|&id| registry.with(|it| *it == id, |it| *it)).collect()
    }

    /// The invariant: any number of stores may be linked at once, each is found by `with` until
    /// its guard drops and never after, and a slot freed by a drop is taken again before the
    /// table grows. Control: a slot cleared behind the registry's back (a link that lost its
    /// store) makes the oracle miss that store, so it is not vacuous.
    #[test]
    fn a_registry_holds_any_number_of_stores_and_finds_each_until_it_is_unlinked() {
        static REGISTRY: Registry<u64> = Registry::new("ty.store_registry.test");
        let count = 3 * SEGMENT as u64 + 5;
        let ids: Vec<u64> = (0..count).collect();
        let mut linked: Vec<Option<Linked<u64>>> =
            ids.iter().map(|&id| Some(REGISTRY.link(Box::new(id)))).collect();
        assert_eq!(found(&REGISTRY, &ids), ids.iter().map(|&id| Some(id)).collect::<Vec<_>>());
        let room = REGISTRY.slots().count();

        // Unlink every other one: those are gone, the rest still found.
        for at in (0..linked.len()).step_by(2) {
            linked[at] = None;
        }
        let expected: Vec<Option<u64>> =
            ids.iter().map(|&id| (id % 2 == 1).then_some(id)).collect();
        assert_eq!(found(&REGISTRY, &ids), expected);

        // Relinking as many takes the freed slots: the table does not grow.
        let again: Vec<Linked<u64>> =
            (0..count.div_ceil(2)).map(|at| REGISTRY.link(Box::new(count + at))).collect();
        assert_eq!(REGISTRY.slots().count(), room);
        drop(again);

        // Control: a store whose slot was cleared is not found (and its guard's drop says so).
        let probe = REGISTRY.link(Box::new(u64::MAX));
        assert_eq!(found(&REGISTRY, &[u64::MAX]), vec![Some(u64::MAX)]);
        let held = probe.slot.swap(std::ptr::null_mut(), std::sync::atomic::Ordering::AcqRel);
        assert_eq!(found(&REGISTRY, &[u64::MAX]), vec![None]);
        probe.slot.store(held, std::sync::atomic::Ordering::Release);
        drop(probe);

        drop(linked);
        assert!(REGISTRY.is_empty());
        assert_eq!(found(&REGISTRY, &ids), vec![None; ids.len()]);
    }
}
