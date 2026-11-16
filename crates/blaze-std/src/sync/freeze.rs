//! Build a value, then share it read-only: [`Building`] (one owner, mutable) becomes
//! [`Frozen`] (many readers, immutable) by [`Building::freeze`], and nothing turns it back
//! (std.md, "Concurrency: what to reach for").
//!
//! ```text
//! let mut table = Building::new_in(Table::new_in(alloc), alloc);
//! table.insert(..);                       // `&mut` while it is being built
//! let table: Frozen<Table, _> = table.freeze();
//! scope.spawn("a", TASK_FRAME, |cx| look(cx, &table));   // a plain `&` read, no lock
//! ```
//!
//! - **Enforced by types.** A `Building` is uniquely owned, so its `&mut` is the borrow
//!   checker's. A `Frozen` gives `&T` only: it has no `get_mut`, no `make_mut`, no `into_inner`,
//!   so there is no way back to a mutable value. `T: Freeze` (core's marker: no `UnsafeCell`
//!   anywhere inside but behind a pointer) is required to freeze, so a frozen value has no shared
//!   mutation in place, which a `Frozen<Atomic<u32>>` would. `Frozen` is `Sync` and `Send` when
//!   `T` and the allocator are.
//! - **Cost.** One allocation at `freeze`, of the value's size, in the allocator `new_in` was
//!   given; a clone of the handle is one counted increment; a read is a pointer load. No lock,
//!   no atomic on a read.
//! - The handle is counted (`blaze_own::Arc`): the value is dropped once, with the last handle.
//!   Readers that all live inside one scope can borrow `&Frozen` (or `&T`) instead and clone
//!   nothing.

use core::{
    alloc::{Allocator, AllocatorClone},
    fmt,
    marker::Freeze,
    ops::{Deref, DerefMut},
};

use blaze_own::{Arc, Refused};

use crate::Plant;

/// A value being built: owned by one, changed through `&mut`. [`Building::freeze`] shares it.
pub struct Building<T, A: AllocatorClone> {
    value: T,
    alloc: A,
    plant: Plant,
}

/// A value frozen by [`Building::freeze`]: read by any number of holders through `&T`, never
/// changed again. A clone is another handle to the same value.
pub struct Frozen<T, A: Allocator> {
    value: Arc<T, A>,
}

impl<T, A: AllocatorClone> Building<T, A> {
    /// `value`, to be built in place and frozen into `alloc`.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn new_in(value: T, alloc: A) -> Building<T, A> {
        Building { value, alloc, plant: Plant::None }
    }

    #[cfg(test)]
    pub(crate) fn planted(value: T, alloc: A, plant: Plant) -> Building<T, A> {
        Building { value, alloc, plant }
    }
}

impl<T: Freeze, A: AllocatorClone> Building<T, A> {
    /// The value frozen: one allocation in the builder's allocator, then shared read-only. An
    /// allocator that refuses fails the job with blaze-own's `AllocFailure` (its convention);
    /// [`Building::try_freeze`] returns the refusal instead.
    pub fn freeze(self) -> Frozen<T, A> {
        let Building { value, alloc, plant } = self;
        frozen(Arc::new_in(value, alloc), plant)
    }

    /// As [`Building::freeze`], or the value back with why the allocator refused.
    pub fn try_freeze(self) -> Result<Frozen<T, A>, Refused<T>> {
        let Building { value, alloc, plant } = self;
        Ok(frozen(Arc::try_new_in(value, alloc)?, plant))
    }
}

fn frozen<T, A: AllocatorClone>(value: Arc<T, A>, plant: Plant) -> Frozen<T, A> {
    if plant == Plant::FreezeLeaks {
        core::mem::forget(Arc::clone(&value));
    }
    Frozen { value }
}

impl<T, A: AllocatorClone> Deref for Building<T, A> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T, A: AllocatorClone> DerefMut for Building<T, A> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

impl<T, A: Allocator> Frozen<T, A> {
    /// How many handles there are now.
    pub fn handles(this: &Frozen<T, A>) -> usize {
        Arc::strong_count(&this.value)
    }

    /// Whether two handles are to one value.
    pub fn ptr_eq(this: &Frozen<T, A>, other: &Frozen<T, A>) -> bool {
        Arc::ptr_eq(&this.value, &other.value)
    }
}

impl<T, A: AllocatorClone> Clone for Frozen<T, A> {
    /// Another handle: one counted increment, nothing copied.
    fn clone(&self) -> Frozen<T, A> {
        Frozen { value: Arc::clone(&self.value) }
    }
}

impl<T, A: Allocator> Deref for Frozen<T, A> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T: fmt::Debug, A: AllocatorClone> fmt::Debug for Building<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Building").field(&self.value).finish()
    }
}

impl<T: fmt::Debug, A: Allocator> fmt::Debug for Frozen<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Frozen").field(&*self.value).finish()
    }
}
