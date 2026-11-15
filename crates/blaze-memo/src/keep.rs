//! What a query's value holds instead of owned memory: slices and values kept in the arena of
//! the run that makes them, freed with it.
//!
//! A kept value may borrow for `'db` (`T: 'db`, not `'static`): references into its own arena
//! and into the arenas its home borrows from. Sound because the type system makes every
//! reference inside `T` outlive `'db`, the returned `&'db T` is no longer than the snapshot
//! borrow, and a value that escapes a run does so as a query value, whose `'db` is the
//! database borrow and whose referents are arenas the home's closure keeps at least as long
//! (`mark_borrows`, `Memo::next`'s carry closure). Nothing reads a kept value's references
//! after `'db` (no destructor runs on it: `!needs_drop`), but a [`keep_owned`] value's
//! destructor, which runs while its arena and the arenas it borrows from are still alive.
//!
//! Every form here but [`keep_owned`] keeps only values that own nothing to drop. That rule
//! fails when the caller is built, not when it is checked: the functions are generic, and a
//! `const` block in a generic function is evaluated only at monomorphization, which
//! `cargo check` does not do. (The macros' assertions, on concrete types, fail at check.)
//! [`keep_owned`] is the one form for a value that owns memory: the counted opt-in, the same
//! drop list a `#[tracked(owns)]` value is on.

use std::alloc::Global;

use crate::{Memo, Plant, arena::with_local, task};

/// `value` kept in the current run's arena, for as long as the snapshot holds it.
///
/// `T` owns no memory to drop (an arena frees what it holds without a destructor): a value
/// that does is kept with [`keep_owned`]. Fails when built, not when checked (the assertion is
/// on a generic `T`, evaluated at monomorphization).
///
/// # Panics
///
/// Outside a query run.
pub fn keep<'db, T: Send + Sync + 'db>(_memo: &'db Memo, value: T) -> &'db T {
    const { crate::plumbing::assert_no_drop::<T>() };
    let slot = task::home_slot();
    let at = with_local(slot, |local| -> *const T { local.alloc(value) });
    // SAFETY: in the run's home arena, which `memo` holds for `'db`.
    unsafe { &*at }
}

/// `value`, which may own memory (a real tree: an expression store's source map, a signature
/// with its store), kept in the current run's arena and listed on the arena's drop list, the
/// one a `#[tracked(owns)]` value and an input's fields are on (`Slot::drop_owned`): its
/// destructor runs once, when the arena drops, and not before; a process that exits runs none.
///
/// Debt against the end state (flat records read in place), as `owns` is: every call site is
/// counted by blaze-memo's `owns` ratchet (`tests/owns.rs`, `owns.txt`), which only goes down.
/// A value that owns nothing is kept with [`keep`].
///
/// # Panics
///
/// Outside a query run.
pub fn keep_owned<'db, T: Send + Sync + 'db>(memo: &'db Memo, value: T) -> &'db T {
    let slot = task::home_slot();
    let at = with_local(slot, |local| -> *const T { local.alloc(value) });
    if memo.plant() != Plant::KeepOwnedUnlisted {
        // SAFETY: `at` is a `T` just placed in `slot`'s arena, listed once here, and never
        // dropped or moved out otherwise (the caller gets only a shared reference). Its `'db`
        // borrows are those of an `owns` query's value, under the same invariant (the module's
        // doc and `relifetime_by_macro`): they point into its own arena, alive while the drop
        // list runs (`Slot::arena` is dropped last), at values listed before this one and so
        // dropped after it (the list runs last first), and into arenas of its declared closure,
        // which outlive it: `Memo::next`'s carry closure drops a reader whenever it drops what
        // it read, and a snapshot releases its arenas downstream first (`memo::Slots`).
        unsafe { slot.drop_owned(at) };
    }
    // SAFETY: as in `keep`.
    unsafe { &*at }
}

/// A copy of `values` kept in the current run's arena.
pub fn keep_slice<'db, T: Copy + Send + Sync + 'db>(_memo: &'db Memo, values: &[T]) -> &'db [T] {
    if values.is_empty() {
        return &[];
    }
    let slot = task::home_slot();
    let at = with_local(slot, |local| -> *const [T] { local.alloc_slice_copy(values) });
    // SAFETY: as in `keep`.
    unsafe { &*at }
}

/// What `values` yields, kept in the current run's arena. The iterator runs first (it may ask
/// queries), into a buffer of the global heap freed before this returns.
///
/// `T` owns no memory to drop, as for [`keep`]: fails when built, not when checked.
pub fn keep_iter<'db, T: Send + Sync + 'db>(
    _memo: &'db Memo,
    values: impl IntoIterator<Item = T>,
) -> &'db [T] {
    const { crate::plumbing::assert_no_drop::<T>() };
    let mut buffer = Vec::new_in(Global);
    buffer.extend(values);
    if buffer.is_empty() {
        return &[];
    }
    let slot = task::home_slot();
    let mut items = buffer.into_iter();
    let len = items.len();
    let at = with_local(slot, |local| -> *const [T] {
        local.alloc_slice_fill_with(len, |_| items.next().expect("as many as counted"))
    });
    // SAFETY: as in `keep`.
    unsafe { &*at }
}

/// A copy of `text` kept in the current run's arena.
pub fn keep_str<'db>(_memo: &'db Memo, text: &str) -> &'db str {
    if text.is_empty() {
        return "";
    }
    let slot = task::home_slot();
    let at = with_local(slot, |local| -> *const str { local.alloc_str(text) });
    // SAFETY: as in `keep`.
    unsafe { &*at }
}
