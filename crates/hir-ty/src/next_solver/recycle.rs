//! What the solver's inference contexts, fulfillment contexts and search graphs allocate, kept
//! per worker between them (docs/v2/plan/lanes/mem/db-solver.md): each is emptied, its capacity
//! kept, when its context ends, and the next one built on the worker takes it, so a warm worker
//! builds contexts without allocating. Obligation lists are kept the same way between their
//! registering and the next list a coercion makes.

use std::{cell::RefCell, mem};

use blaze_mem::{
    Tag,
    scratch::{Shelf, Shelved},
    tag,
};
use rustc_next_trait_solver::solve::SearchStorage;

use crate::next_solver::{
    DbInterner,
    fulfill::Pending,
    infer::{
        InferTables,
        traits::{PredicateObligation, PredicateObligations},
    },
};

/// Kept up to the deepest nesting of canonical goals (each nested goal's context is alive
/// while it solves), and of bodies inferred inside another's goals.
const INFER_DEPTH: usize = 32;
/// Fulfillment contexts nest as inference contexts do: one per `ObligationCtxt` alive.
const PENDING_DEPTH: usize = 32;
/// A search graph lives for one root goal; root goals nest only through a nested
/// `InferCtxt`'s own solving.
const SEARCH_DEPTH: usize = 8;
/// Obligation lists are registered as soon as they are made: one is alive per coercion or
/// relation in progress.
const OBLIGATIONS_DEPTH: usize = 8;

/// Storage bigger than this is freed, not kept: a huge body's root context would pin its high
/// water on every worker. Such a body pays its tables' few allocations per run, against at
/// least this much work.
pub(crate) const KEEP_AT_MOST: usize = 4 << 20;

/// What a shelf keeps: emptied in place, its capacity kept, and measured.
trait Recycled: Default + 'static {
    /// Drops every value, keeping capacity.
    fn empty(&mut self);

    /// Bytes it holds room for.
    fn held(&self) -> usize;
}

impl Recycled for InferTables<'static> {
    fn empty(&mut self) {
        InferTables::empty(self);
    }

    fn held(&self) -> usize {
        InferTables::held(self)
    }
}

impl Recycled for Pending<'static> {
    fn empty(&mut self) {
        self.clear();
    }

    fn held(&self) -> usize {
        Pending::held(self)
    }
}

impl Recycled for SearchStorage<DbInterner<'static>> {
    /// `SearchGraph::into_storage` empties it.
    fn empty(&mut self) {}

    fn held(&self) -> usize {
        SearchStorage::held(self) + self.scratch().held()
    }
}

impl Recycled for PredicateObligations<'static> {
    fn empty(&mut self) {
        self.clear();
    }

    fn held(&self) -> usize {
        self.capacity() * size_of::<PredicateObligation<'static>>()
    }
}

/// A shelved value, its bytes counted into `Tag::Infer` while it is on a shelf and uncounted
/// when it leaves, taken or dropped (by a full shelf or an ending thread), so the tag shows the
/// solver's resident scratch.
#[derive(Default)]
struct Kept<T: Recycled> {
    value: T,
    counted: usize,
}

impl<T: Recycled> Kept<T> {
    fn new(value: T, counted: usize) -> Kept<T> {
        tag::add(Tag::Infer, counted);
        Kept { value, counted }
    }

    fn into_value(mut self) -> T {
        tag::sub(Tag::Infer, mem::take(&mut self.counted));
        mem::take(&mut self.value)
    }
}

impl<T: Recycled> Drop for Kept<T> {
    fn drop(&mut self) {
        if self.counted != 0 {
            tag::sub(Tag::Infer, self.counted);
        }
    }
}

macro_rules! shelf {
    ($shelf:ident: $ty:ty, $depth:expr) => {
        thread_local! {
            static $shelf: RefCell<Shelf<Kept<$ty>, $depth>> = const { RefCell::new(Shelf::new()) };
        }

        impl Shelved for Kept<$ty> {
            fn empty(&mut self) {
                self.value.empty();
            }

            fn held(&self) -> usize {
                self.counted
            }

            fn unshelve() -> Option<Self> {
                $shelf.try_with(|shelf| shelf.borrow_mut().take()).ok().flatten()
            }

            fn shelve(self) {
                // A thread that is ending drops it, uncounting it.
                let _ = $shelf.try_with(|shelf| shelf.borrow_mut().give(self));
            }
        }
    };
}

shelf!(INFER: InferTables<'static>, INFER_DEPTH);
shelf!(PENDING: Pending<'static>, PENDING_DEPTH);
shelf!(SEARCH: SearchStorage<DbInterner<'static>>, SEARCH_DEPTH);
shelf!(OBLIGATIONS: PredicateObligations<'static>, OBLIGATIONS_DEPTH);

fn take<T: Recycled>() -> Option<T>
where
    Kept<T>: Shelved,
{
    Kept::<T>::unshelve().map(Kept::into_value)
}

/// Empties `value` and shelves it, unless it holds more than [`KEEP_AT_MOST`].
fn give<T: Recycled>(mut value: T)
where
    Kept<T>: Shelved,
{
    value.empty();
    let held = value.held();
    if held > KEEP_AT_MOST {
        return;
    }
    Kept::new(value, held).shelve();
}

/// `B` from `A`, where `B` is `A` with `'db` replaced by another lifetime.
///
/// # Safety
///
/// `B` must be `A` with only a lifetime changed (so the same layout), and the value must hold no
/// value of a type naming the lifetime: every container in it is empty.
unsafe fn erase_empty<A, B>(value: A) -> B {
    const { assert!(size_of::<A>() == size_of::<B>() && align_of::<A>() == align_of::<B>()) };
    let value = mem::ManuallyDrop::new(value);
    // SAFETY: the caller's precondition: `B` is `A` with one lifetime replaced, and the value
    // holds no data of either lifetime, so no reference of `'db` outlives it.
    unsafe { mem::transmute_copy(&*value) }
}

/// Empty tables for a new inference context: the worker's, or new ones.
pub(crate) fn infer_tables<'db>() -> InferTables<'db> {
    match take::<InferTables<'static>>() {
        // SAFETY: shelved tables were emptied (`give`), and `InferTables<'db>` is
        // `InferTables<'static>` with only the lifetime changed.
        Some(tables) => unsafe { erase_empty(tables) },
        None => InferTables::default(),
    }
}

/// Puts an ended context's tables on the worker's shelf, emptied.
pub(crate) fn give_infer_tables(mut tables: InferTables<'_>) {
    tables.empty();
    // SAFETY: `empty` dropped every value, so the tables hold no `'db` data, and
    // `InferTables<'static>` is the same type up to the lifetime.
    give::<InferTables<'static>>(unsafe { erase_empty(tables) });
}

/// Empty pending columns for a new fulfillment context: the worker's, or new ones.
pub(crate) fn pending<'db>() -> Pending<'db> {
    match take::<Pending<'static>>() {
        // SAFETY: shelved columns were cleared (`give`), and `Pending<'db>` is
        // `Pending<'static>` with only the lifetime changed.
        Some(pending) => unsafe { erase_empty(pending) },
        None => Pending::default(),
    }
}

/// Puts an ended fulfillment context's columns on the worker's shelf, cleared.
pub(crate) fn give_pending(mut pending: Pending<'_>) {
    // Drops every row's `StalledOn`, the only column with heap data.
    pending.clear();
    // SAFETY: `clear` dropped every row, so the columns hold no `'db` data, and
    // `Pending<'static>` is the same type up to the lifetime.
    give::<Pending<'static>>(unsafe { erase_empty(pending) });
}

/// Empty search graph storage for a root goal: the worker's, or new storage.
pub(crate) fn search_storage<'db>() -> SearchStorage<DbInterner<'db>> {
    match take::<SearchStorage<DbInterner<'static>>>() {
        // SAFETY: shelved storage was emptied by `SearchGraph::into_storage`, and
        // `SearchStorage<DbInterner<'db>>` is the `'static` one with only the lifetime changed.
        Some(storage) => unsafe { erase_empty(storage) },
        None => SearchStorage::default(),
    }
}

/// Puts a root goal's search graph storage, emptied by `SearchGraph::into_storage`, on the
/// worker's shelf.
pub(crate) fn give_search_storage(storage: SearchStorage<DbInterner<'_>>) {
    // SAFETY: `into_storage` emptied the stack, the provisional cache, its spare lists and the
    // scratch pools, so the storage holds no `'db` data, and the `'static` type differs only in
    // the lifetime.
    give::<SearchStorage<DbInterner<'static>>>(unsafe { erase_empty(storage) });
}

/// An empty obligation list with room for `capacity`: the worker's, or a new one.
pub(crate) fn obligations<'db>(capacity: usize) -> PredicateObligations<'db> {
    match take::<PredicateObligations<'static>>() {
        Some(list) => {
            // SAFETY: shelved lists were cleared (`give`), and `PredicateObligations<'db>` is
            // `PredicateObligations<'static>` with only the lifetime changed.
            let mut list: PredicateObligations<'db> = unsafe { erase_empty(list) };
            // alloc: only when the worker's list is shorter than `capacity`
            list.reserve(capacity);
            list
        }
        // alloc: a worker's first list, kept from then on
        None => PredicateObligations::with_capacity(capacity),
    }
}

/// Puts a registered obligation list on the worker's shelf, cleared; one that never had room
/// is dropped.
pub(crate) fn give_obligations(mut list: PredicateObligations<'_>) {
    if list.capacity() == 0 {
        return;
    }
    list.clear();
    // SAFETY: `clear` dropped every obligation, so the list holds no `'db` data, and
    // `PredicateObligations<'static>` is the same type up to the lifetime.
    give::<PredicateObligations<'static>>(unsafe { erase_empty(list) });
}

#[cfg(test)]
mod tests {
    use super::{KEEP_AT_MOST, PENDING_DEPTH, give_obligations, give_pending, obligations, pending};
    use crate::next_solver::{fulfill::Pending, infer::traits::PredicateObligations};

    fn reserved(rows: usize) -> Pending<'static> {
        let mut pending = Pending::default();
        pending.reserve_for_tests(rows);
        pending
    }

    /// Each test runs on a thread of its own, so its shelves start empty.
    #[test]
    fn a_shelved_value_comes_back_with_its_capacity() {
        let held = reserved(64).held();
        give_pending(reserved(64));
        assert_eq!(pending().held(), held);
        assert_eq!(pending().held(), 0, "the shelf held one");
    }

    #[test]
    fn a_registered_obligation_list_comes_back_with_its_capacity() {
        give_obligations(PredicateObligations::<'static>::new());
        give_obligations(PredicateObligations::<'static>::with_capacity(4));
        let list: PredicateObligations<'static> =
            blaze_alloc_count::assert_at_most(0, "a kept obligation list", || obligations(2));
        assert!(list.capacity() >= 4);
        let next: PredicateObligations<'static> = obligations(0);
        assert_eq!(next.capacity(), 0, "a list without room is not kept");
    }

    #[test]
    fn a_huge_value_is_not_kept() {
        let huge = reserved(KEEP_AT_MOST);
        assert!(huge.held() > KEEP_AT_MOST);
        give_pending(huge);
        assert_eq!(pending().held(), 0);
    }

    /// A full shelf keeps its largest.
    #[test]
    fn a_full_shelf_keeps_its_largest() {
        for rows in 1..=PENDING_DEPTH + 4 {
            give_pending(reserved(rows));
        }
        let smallest_kept = reserved(5).held();
        for _ in 0..PENDING_DEPTH {
            assert!(pending().held() >= smallest_kept);
        }
        assert_eq!(pending().held(), 0);
    }
}
