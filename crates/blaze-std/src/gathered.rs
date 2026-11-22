//! [`Gathered`]: one value per worker, written with no lock while tasks run, merged once when
//! no task can hold one (D179's GATHERED shape; docs/v2/plan/design/end-state.md).
//!
//! ```text
//! // main, once: the column registered with the workers
//! let mut found = Gathered::<Findings>::register(&mut opts, SlotOpts { name: "lint.findings", alloc, make });
//! // a task: its worker's cell, no lock
//! cx.on_worker(|w| found.with(w, |cell| cell.push(finding)));
//! // after the scope that ran the tasks: the merge, in an order no worker decides
//! found.gather_sorted(&workers, alloc, |cell, out| cell.drain_into(out), |key, finding| all.push(key, finding))?;
//! ```
//!
//! - **A column of the workers' cells** ([`crate::workers`], D171): a `Slot<T>` that `Gathered`
//!   keeps private, so the only way to a cell is [`Gathered::with`], under `&self`, inside
//!   `Cx::on_worker` (which holds the worker's cell for the closure alone).
//! - **The gather's proof is `&mut Gathered`.** [`Gathered::gather`] and
//!   [`Gathered::gather_sorted`] take `&mut self`: while any task can still be inside `with`
//!   it holds `&Gathered` (borrowed into its scope), and the borrow checker refuses the `&mut`.
//!   So a gather while a worker holds a cell does not compile; nothing is checked at run time.
//!   The boundaries that give it, each the end of every borrow the tasks took:
//!   - the end of a task scope ([`crate::task`]'s `scope`, `par`): the scope's tasks borrowed
//!     `&Gathered` from the enclosing frame and the scope joined them all;
//!   - the end of a run: `rt::run_with_workers`'s `main` returned, so its tasks have;
//!   - a driver's tick, when the tick ran its tasks in a scope it ended.
//!   A `Gathered` moved into a `'static` task (shared by `Shared`/`Arc`) can give no `&mut`, so
//!   it can never be gathered: hold it in the frame that scopes the tasks.
//! - **The merge order is no worker's.** Cells are visited in worker order, so a merge that
//!   depends on its order would make a result that depends on which worker ran what. Two forms
//!   only: [`Gathered::gather`] for a `T: OrderFree` (a type that states its merge is
//!   commutative and associative: counts, sums, maxima, sets), and [`Gathered::gather_sorted`],
//!   which drains every cell's `(K, V)` items into one buffer, sorts them by `(K, V)`, and hands
//!   them to the merge in that order. Neither hands the merge a `WorkerId` or a cell's
//!   position. A type that is not `OrderFree` has no `gather`, so a worker-order merge cannot be
//!   written through this type.

use core::{alloc::Allocator, fmt};

use blaze_own::{AllocFailure, Vec};

use crate::workers::{Slot, SlotOpts, Worker, WorkerId, Workers, WorkersOpts};

/// A type whose cells merge to one result in any order: its merge is commutative and
/// associative (a count, a sum, a maximum, a set union). Stated by the type, not inferred; a
/// type whose merge depends on order uses [`Gathered::gather_sorted`].
pub trait OrderFree {}

impl OrderFree for u64 {}
impl OrderFree for u32 {}
impl OrderFree for usize {}

/// A planted bug for the simulation's control (rule 53).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum GatherPlant {
    None,
    /// `gather_sorted` hands the items in the order the cells were visited: worker order.
    WorkerOrder,
}

impl blaze_own::planted::Plants for GatherPlant {}

/// Why a gather gave nothing.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum GatherError {
    /// The buffer the sorted gather drains into could not grow.
    Buffer { gathered: &'static str, why: AllocFailure },
}

impl fmt::Display for GatherError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GatherError::Buffer { gathered, why } => write!(f, "bstd::gathered {gathered}: its sorted gather's buffer: {why}"),
        }
    }
}

impl core::error::Error for GatherError {}

/// One `T` per worker, gathered once no task can hold one; see the module's doc. Not `Clone`.
pub struct Gathered<T> {
    slot: Slot<T>,
    name: &'static str,
    plant: GatherPlant,
}

impl<T: Send + 'static> Gathered<T> {
    /// Registers the column: each worker's cell is `slot.make(worker, alloc)`, made at the
    /// worker's first `on_worker`. Called by main, once, before the workers are sealed.
    pub fn register<F>(opts: &mut WorkersOpts, slot: SlotOpts<F>) -> Gathered<T>
    where
        F: Fn(WorkerId, blaze_own::Alloc<'_>) -> T + Copy + Send + Sync + 'static,
    {
        Gathered::planted(opts, slot, GatherPlant::None)
    }

    pub(crate) fn planted<F>(opts: &mut WorkersOpts, slot: SlotOpts<F>, plant: GatherPlant) -> Gathered<T>
    where
        F: Fn(WorkerId, blaze_own::Alloc<'_>) -> T + Copy + Send + Sync + 'static,
    {
        let name = slot.name;
        Gathered { slot: opts.slot(slot), name, plant }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    /// `f` on this worker's cell: no lock, the worker's alone while `f` runs.
    #[inline]
    pub fn with<R>(&self, w: &mut Worker<'_>, f: impl FnOnce(&mut T) -> R) -> R {
        f(w.slot(self.slot))
    }

    /// Every made cell merged by `merge`, for a type whose merge has no order.
    ///
    /// ```compile_fail
    /// # use bstd::gathered::Gathered;
    /// # fn scoped(found: &mut Gathered<u64>, workers: &bstd::workers::Workers, run: impl FnOnce(&Gathered<u64>)) {
    /// let shared = &*found;                    // a task scope's borrow
    /// found.gather(workers, |_| {});           // refused: `found` is borrowed by the tasks
    /// run(shared);
    /// # }
    /// ```
    pub fn gather(&mut self, workers: &Workers, merge: impl FnMut(&mut T))
    where
        T: OrderFree,
    {
        // SAFETY: `&mut self` borrows the only key out of every `with`: no cell of this column
        // is referred to while this runs.
        unsafe { workers.each_made(self.slot, merge) }
    }

    /// Every made cell's items, drained by `drain` (which hands each `(K, V)` to its sink),
    /// sorted by `(K, V)` in a buffer in `alloc`, and handed to `merge` in that order: a
    /// result no worker assignment changes.
    pub fn gather_sorted<K: Ord, V: Ord, A: Allocator>(
        &mut self,
        workers: &Workers,
        alloc: A,
        mut drain: impl FnMut(&mut T, &mut dyn FnMut(K, V)),
        mut merge: impl FnMut(K, V),
    ) -> Result<(), GatherError> {
        let mut items: Vec<(K, V), A> = Vec::new_in(alloc);
        let mut refused = None;
        // SAFETY: as in `gather`.
        unsafe {
            workers.each_made(self.slot, |cell| {
                drain(cell, &mut |key, value| {
                    if refused.is_some() {
                        return;
                    }
                    if let Err(failed) = items.try_push((key, value)) {
                        refused = Some(failed.why);
                    }
                });
            });
        }
        if let Some(why) = refused {
            return Err(GatherError::Buffer { gathered: self.name, why });
        }
        if self.plant != GatherPlant::WorkerOrder {
            items.as_mut_slice().sort_unstable();
        }
        for (key, value) in items.drain(..) {
            merge(key, value);
        }
        Ok(())
    }
}

impl<T> fmt::Debug for Gathered<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Gathered({})", self.name)
    }
}

#[cfg(test)]
mod tests;
