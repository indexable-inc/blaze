//! The heap a value owns, in bytes, for the weighers of the queries kept by their bytes
//! (`#[blaze_query::tracked(lru, weigh = ..)]`): what dropping the value frees, not its own size.

use la_arena::{Arena, ArenaMap, Idx};
use thin_vec::ThinVec;

/// A `Vec`'s buffer, its capacity's elements.
pub(crate) fn vec_bytes<T>(vec: &Vec<T>) -> usize {
    vec.capacity() * size_of::<T>()
}

/// A `ThinVec`'s buffer, its header (length and capacity) and its capacity's elements; none
/// while it has no capacity (the shared empty header).
pub(crate) fn thin_vec_bytes<T>(vec: &ThinVec<T>) -> usize {
    match vec.capacity() {
        0 => 0,
        capacity => 2 * size_of::<usize>() + capacity * size_of::<T>(),
    }
}

/// An `ArenaMap`'s slots up to its last value (la_arena shows neither its length nor its
/// capacity, so an approximation), and what `value` says each value owns.
pub(crate) fn arena_map_bytes<T, V>(
    map: &ArenaMap<Idx<T>, V>,
    value: impl Fn(&V) -> usize,
) -> usize {
    let slots = map.iter().next_back().map_or(0, |(idx, _)| idx.into_raw().into_u32() as usize + 1);
    slots * size_of::<Option<V>>() + map.values().map(value).sum::<usize>()
}

/// An `Arena`'s values by its length (la_arena shows no capacity), not each value's own heap.
pub(crate) fn arena_bytes<T>(arena: &Arena<T>) -> usize {
    arena.len() * size_of::<T>()
}
