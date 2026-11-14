//! What a query's value holds instead of owned memory: slices and values kept in the arena of
//! the run that makes them, freed with it.

use std::alloc::Global;

use crate::{Memo, arena::with_local, task};

/// `value` kept in the current run's arena, for as long as the snapshot holds it.
///
/// # Panics
///
/// Outside a query run.
pub fn keep<'db, T: Send + Sync + 'static>(_memo: &'db Memo, value: T) -> &'db T {
    const { crate::plumbing::assert_no_drop::<T>() };
    let slot = task::home_slot();
    let at = with_local(slot, |local| -> *const T { local.alloc(value) });
    // SAFETY: in the run's home arena, which `memo` holds for `'db`.
    unsafe { &*at }
}

/// A copy of `values` kept in the current run's arena.
pub fn keep_slice<'db, T: Copy + Send + Sync + 'static>(_memo: &'db Memo, values: &[T]) -> &'db [T] {
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
pub fn keep_iter<'db, T: Send + Sync + 'static>(
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
